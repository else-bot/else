use crate::controls::{Control as _, Scale, Tier};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Sizable as _, StyledExt as _, Theme,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::super::action_card::{OPTION_HEIGHT, footer_note};
use super::{Connection, ElseApp, QuestionForm, RowKey, SubmitState, callout, letter, receipt};
use crate::model::{Question, QuestionAnswer};

/// The field on a pending single-choice card for an answer its options don't
/// cover, and the reply field of an open question. Its text is sent as an
/// owner message, which skips the card and continues the conversation like a
/// reply in the composer.
pub(in crate::app) struct WriteIn {
    input: Entity<InputState>,
    _enter: Subscription,
}

fn takes_write_in(question: &Question) -> bool {
    question.is_pending() && !question.is_approval() && !question.multiple
}

impl ElseApp {
    pub(in crate::app) fn sync_write_ins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let open: Vec<(Uuid, bool)> = view
            .conversation
            .iter()
            .flat_map(|c| c.questions())
            .filter(|q| takes_write_in(q))
            .map(|q| (q.id, q.choices.is_empty()))
            .collect();
        view.write_ins
            .retain(|id, _| open.iter().any(|(open, _)| open == id));
        for (id, reply) in open {
            view.write_ins.entry(id).or_insert_with(|| {
                let input = cx.new(|cx| {
                    InputState::new(window, cx).placeholder(if reply {
                        "Write your answer"
                    } else {
                        "Type your own answer"
                    })
                });
                let _enter = cx.subscribe_in(
                    &input,
                    window,
                    move |app, _, event: &InputEvent, window, cx| match event {
                        InputEvent::PressEnter { .. } => app.send_write_in(id, window, cx),
                        InputEvent::Change => app.remeasure_row(RowKey::Question(id), cx),
                        _ => {}
                    },
                );
                WriteIn { input, _enter }
            });
        }
    }

    fn send_write_in(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_ref() else {
            return;
        };
        let open = view
            .conversation
            .as_ref()
            .and_then(|c| c.question(id))
            .is_some_and(takes_write_in)
            && !view
                .question_submits
                .get(&id)
                .is_some_and(QuestionForm::locks);
        let Some(input) = view.write_ins.get(&id).map(|w| w.input.clone()) else {
            return;
        };
        let text = input.read(cx).value().trim().to_owned();
        if !open || text.is_empty() {
            return;
        }
        if self.enqueue(view.bot_id, text, Vec::new(), None, window, cx) {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
    }

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
        bot_name: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let id = question.id;
        if !question.is_pending() || form.is_some_and(QuestionForm::locks) {
            let reply = view
                .conversation
                .as_ref()
                .and_then(|c| c.skipping_reply(question))
                .map(|item| item.text.as_str());
            let expanded = view.expanded_questions.contains(&id);
            let settled = settled(question, form, reply);
            let color = if settled.resumed {
                theme.success
            } else {
                theme.muted_foreground
            };
            let mut card = v_flex()
                .w_full()
                .rounded(theme.radius_lg)
                .bg(theme.secondary)
                .child(
                    receipt(
                        id,
                        settled.resumed,
                        question.title.clone(),
                        settled.outcome,
                        expanded,
                        &theme,
                    )
                    .when(expanded && question.context.is_some(), |button| {
                        button.rounded_b(px(0.))
                    })
                    .on_click(cx.listener(move |app, _, _, cx| {
                        app.toggle_question_details(id, cx);
                    })),
                );
            if expanded && let Some(context) = &question.context {
                card = card.child(
                    div()
                        .w_full()
                        .px_4()
                        .pb_4()
                        .text_sm()
                        .whitespace_normal()
                        .text_color(theme.muted_foreground)
                        .child(context.clone()),
                );
            }
            let at = question
                .settled_at
                .map(|at| {
                    at.with_timezone(&chrono::Local)
                        .format(" · %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            return v_flex()
                .w_full()
                .gap_2()
                .child(divider(format!("{}{at}", settled.marker), color))
                .child(card)
                .into_any_element();
        }
        let locked = self.connection != Connection::Connected;
        let answer = form.map(|f| &f.answer);
        let selected: Vec<_> = question
            .choices
            .iter()
            .filter(|choice| answer.is_some_and(|a| a.contains(&choice.id)))
            .collect();
        let kind = if question.multiple {
            "Choose any"
        } else if question.choices.is_empty() {
            "Reply here"
        } else {
            "Choose one"
        };
        let mut body = super::super::action_card::card(
            IconName::CircleQuestionMark,
            question.title.clone(),
            format!("{bot_name} · {kind}"),
        );
        if let Some(context) = &question.context {
            body = body.child(
                div()
                    .text_sm()
                    .whitespace_normal()
                    .text_color(theme.muted_foreground)
                    .child(context.clone()),
            );
        }
        let mut rows = v_flex().w_full().gap_2();
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
                        .min_h(px(OPTION_HEIGHT))
                        .px_3()
                        .py_2()
                        .gap_2p5()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(if checked {
                            theme.primary.opacity(0.5)
                        } else {
                            theme.border
                        })
                        .when(!locked, |row| row.cursor_pointer())
                        .when(checked, |row| row.bg(theme.primary.opacity(0.08)))
                        .when(!locked && !checked, |row| {
                            row.hover(|style| style.bg(theme.muted.opacity(0.5)))
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
                        .min_h(px(OPTION_HEIGHT))
                        .px_2()
                        .py_1p5()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .when(!locked, |row| row.cursor_pointer())
                        .accessibility_label(format!("{}. {}", letter(&choice.id), choice.label))
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2p5()
                                .child(key_chip(&theme, letter(&choice.id)))
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
        if let Some(write_in) = view.write_ins.get(&id).filter(|_| takes_write_in(question)) {
            rows = rows.child(self.render_write_in(id, question, write_in, locked, cx));
        }
        body = body.child(rows);
        let hint = if question.multiple {
            "Choose one or more, then click Done. Sending a new message skips this.".to_owned()
        } else if question.choices.is_empty() {
            format!("Your answer is sent as a message, and {bot_name} continues from it.")
        } else {
            "Choose an answer or type your own. Sending a new message skips this.".to_owned()
        };
        let mut footer = super::super::action_card::footer().child(footer_note(&theme, hint));
        if question.multiple {
            let answer = QuestionAnswer::Multiple(selected.iter().map(|c| c.id.clone()).collect());
            let disabled = locked || selected.is_empty();
            footer = footer.child(
                Button::new(SharedString::from(format!("question-{id}-done")))
                    .control(Tier::Primary, Scale::Compact, disabled, cx)
                    .label(format!("Done · {} selected", selected.len()))
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.answer_question(id, answer.clone(), window, cx);
                    })),
            );
        }
        if let Some(SubmitState::Failed(error)) = form.map(|f| &f.state) {
            body = body.child(callout(error.clone(), &theme));
        }
        body.footer(footer).into_any_element()
    }

    fn render_write_in(
        &self,
        id: Uuid,
        question: &Question,
        write_in: &WriteIn,
        locked: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let next = question
            .choices
            .last()
            .and_then(|c| c.id.chars().next())
            .and_then(|c| char::from_u32(c as u32 + 1))
            .map(|c| letter(&c.to_string()));
        let empty = write_in.input.read(cx).value().trim().is_empty();
        let disabled = locked || empty;
        h_flex()
            .w_full()
            .min_h(px(OPTION_HEIGHT))
            .gap_2p5()
            .pl_2()
            .pr_1()
            .py_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .children(next.map(|next| key_chip(theme, next)))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&write_in.input)
                        .focus_ring(false)
                        .appearance(false)
                        .small()
                        .px_0()
                        .aria_label("Your own answer")
                        .disabled(locked),
                ),
            )
            .child(
                Button::new(SharedString::from(format!("question-{id}-write-in")))
                    .control(Tier::Primary, Scale::Compact, disabled, cx)
                    .label("Send")
                    .children(Keystroke::parse("enter").ok().map(|key| {
                        div()
                            .flex_none()
                            .text_xs()
                            .child(Kbd::new(key).appearance(false))
                    }))
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.send_write_in(id, window, cx);
                    })),
            )
    }
}

/// The letter that leads an option row.
fn key_chip(theme: &Theme, key: String) -> Div {
    h_flex()
        .size_6()
        .flex_shrink_0()
        .justify_center()
        .rounded(theme.radius)
        .bg(theme.muted)
        .text_color(theme.muted_foreground)
        .text_xs()
        .child(key)
}

/// How a settled card reads: the divider above it, whether the bot carried
/// on with an answer, and the receipt's outcome line.
#[derive(Debug, PartialEq, Eq)]
struct Settled {
    marker: &'static str,
    resumed: bool,
    outcome: String,
}

/// The longest reply a receipt quotes before cutting it short.
const QUOTE_BYTES: usize = 240;

fn settled(question: &Question, form: Option<&QuestionForm>, reply: Option<&str>) -> Settled {
    let resumed = |outcome: String| Settled {
        marker: "Resumed",
        resumed: true,
        outcome,
    };
    if question.is_pending() {
        return Settled {
            marker: "Answer sent",
            resumed: true,
            outcome: if form.is_some_and(|f| f.state == SubmitState::Sending) {
                "Sending…".into()
            } else {
                chose(question, form.map(|f| &f.answer))
            },
        };
    }
    match question.status.as_str() {
        "answered" => resumed(chose(question, question.answer.as_ref())),
        "skipped" => match reply.map(str::trim).filter(|r| !r.is_empty()) {
            Some(reply) => {
                let quote = if reply.len() > QUOTE_BYTES {
                    let end = reply.floor_char_boundary(QUOTE_BYTES);
                    format!("{}…", reply[..end].trim_end())
                } else {
                    reply.to_owned()
                };
                resumed(format!("You answered in chat: “{quote}”"))
            }
            None => Settled {
                marker: "Continued in chat",
                resumed: false,
                outcome: "Skipped: you sent a new message instead.".into(),
            },
        },
        _ => Settled {
            marker: "Withdrawn",
            resumed: false,
            outcome: "Withdrawn: the request ended before you answered.".into(),
        },
    }
}

fn chose(question: &Question, answer: Option<&QuestionAnswer>) -> String {
    let chosen: Vec<_> = question
        .choices
        .iter()
        .filter(|c| answer.is_some_and(|a| a.contains(&c.id)))
        .collect();
    match chosen.as_slice() {
        [] => "Answer sent.".into(),
        [one] if !question.multiple => format!("You chose {} · {}.", letter(&one.id), one.label),
        many => format!(
            "You chose {}.",
            many.iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// A full-width rule with a label that marks where a bot paused for the
/// owner, and how it went on.
fn divider(label: String, color: Hsla) -> Div {
    let rule = || div().flex_1().h(px(1.)).bg(color.opacity(0.35));
    h_flex()
        .w_full()
        .gap_2p5()
        .items_center()
        .text_xs()
        .font_medium()
        .text_color(color)
        .child(rule())
        .child(div().flex_none().child(label))
        .child(rule())
}

#[cfg(test)]
mod tests {
    use super::{QuestionForm, Settled, SubmitState, settled};
    use crate::model::{Question, QuestionAnswer, QuestionChoice};
    use uuid::Uuid;

    fn question(status: &str, multiple: bool, answer: Option<QuestionAnswer>) -> Question {
        Question {
            id: Uuid::nil(),
            kind: "choice".into(),
            title: "Which format?".into(),
            action: None,
            context: None,
            choices: ["Plain text", "HTML digest", "Both"]
                .iter()
                .zip(["a", "b", "c"])
                .map(|(label, id)| QuestionChoice {
                    id: id.into(),
                    label: (*label).into(),
                })
                .collect(),
            multiple,
            status: status.into(),
            answer,
            settled_at: None,
            used: None,
        }
    }

    #[test]
    fn a_settled_card_says_whether_the_bot_resumed_and_with_what() {
        let single = question("answered", false, Some(QuestionAnswer::Single("b".into())));
        assert_eq!(
            settled(&single, None, None),
            Settled {
                marker: "Resumed",
                resumed: true,
                outcome: "You chose B · HTML digest.".into()
            }
        );
        let many = question(
            "answered",
            true,
            Some(QuestionAnswer::Multiple(vec!["a".into(), "c".into()])),
        );
        assert_eq!(
            settled(&many, None, None).outcome,
            "You chose Plain text, Both."
        );
        let skipped = question("skipped", false, None);
        let own = settled(&skipped, None, Some("Markdown, for Notion "));
        assert!(own.resumed);
        assert_eq!(own.outcome, "You answered in chat: “Markdown, for Notion”");
        let long = "é".repeat(300);
        assert!(settled(&skipped, None, Some(&long)).outcome.ends_with("…”"));
        assert!(!settled(&skipped, None, None).resumed);
        assert_eq!(
            settled(&question("cancelled", false, None), None, None).marker,
            "Withdrawn"
        );
    }

    #[test]
    fn a_sent_answer_reads_as_sent_before_the_server_settles_it() {
        let pending = question("pending", false, None);
        let mut form = QuestionForm {
            answer: QuestionAnswer::Single("a".into()),
            state: SubmitState::Sending,
        };
        assert_eq!(settled(&pending, Some(&form), None).outcome, "Sending…");
        form.state = SubmitState::Recorded;
        assert_eq!(
            settled(&pending, Some(&form), None).outcome,
            "You chose A · Plain text."
        );
    }
}
