//! An approval card: the action a bot wants to take, the exact command or
//! request, and how long the approval lasts. The owner picks one scope from
//! the choices the server offered, so the main button says exactly what
//! they agree to.

use super::*;
use crate::controls::{Scale, Tier};
use crate::model::QuestionChoice;
use gpui_kit::component::radio::Radio;

/// How long an approval choice lasts, as the scope list shows it.
fn scope(choice: &QuestionChoice, question: &Question, bot: &str) -> (String, String) {
    match choice.id.as_str() {
        APPROVE => (
            "Just this once".into(),
            "Covers exactly this action.".into(),
        ),
        APPROVE_TASK => (
            "Always for this task".into(),
            question.task_scope().map_or_else(
                || format!("Remembers this task and action type for {bot}'s future chats and scheduled routine runs."),
                |scope| {
                    format!(
                        "Remembers {} on {} for {bot}'s future chats and scheduled routine runs.",
                        scope.task, scope.target
                    )
                },
            ),
        ),
        APPROVE_TOOL => (
            question.mcp_tool_scope().map_or_else(
                || "Always allow this tool".to_owned(),
                |(integration, tool)| format!("Always allow {integration} · {tool}"),
            ),
            format!("For this action type in {bot}'s chat requests."),
        ),
        APPROVE_ROUTINE => (
            "Always for this routine".into(),
            "Allows destructive actions for its saved task. Changing the task clears this.".into(),
        ),
        APPROVE_FOREVER => (
            format!("Always for {bot}"),
            format!("{bot} can take destructive actions without asking."),
        ),
        _ => (choice.label.clone(), String::new()),
    }
}

/// The context lines shown before "Show all".
const PREVIEW_LINES: usize = 6;
const PREVIEW_BYTES: usize = 480;

/// The first lines of a long context, or `None` when it fits whole. A line
/// cut partway ends in an ellipsis; "Show all" marks the hidden lines.
fn preview(context: &str) -> Option<String> {
    let lines = context
        .match_indices('\n')
        .nth(PREVIEW_LINES - 1)
        .map_or(context.len(), |(ix, _)| ix);
    if lines > PREVIEW_BYTES {
        let end = context.floor_char_boundary(PREVIEW_BYTES);
        return Some(format!("{}…", context[..end].trim_end()));
    }
    (lines < context.len()).then(|| context[..lines].trim_end().to_owned())
}

impl ElseApp {
    fn choose_approval_scope(&mut self, question: Uuid, choice: String, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if view
            .question_submits
            .get(&question)
            .is_some_and(QuestionForm::locks)
        {
            return;
        }
        view.approval_scopes.insert(question, choice);
        self.remeasure_row(RowKey::Question(question), cx);
    }

    pub(super) fn render_approval_card(
        &self,
        question: &Question,
        submit: Option<&QuestionForm>,
        bot_name: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let id = question.id;
        let expanded = view.expanded_questions.contains(&id);
        let note = |text: String| {
            div()
                .text_xs()
                .whitespace_normal()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        if !question.is_pending() {
            let approved = question.status == "answered"
                && question
                    .answer
                    .as_ref()
                    .and_then(QuestionAnswer::single)
                    .is_some_and(|answer| answer != DENY);
            let mut card = v_flex()
                .w_full()
                .rounded(theme.radius_lg)
                .bg(theme.secondary)
                .child(
                    receipt(
                        id,
                        approved,
                        question.title.clone(),
                        outcome(question, bot_name),
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
                        .child(super::super::action_card::body_text(context.clone())),
                );
            }
            return card.into_any_element();
        }
        let locked =
            submit.is_some_and(QuestionForm::locks) || self.connection != Connection::Connected;
        let destructive = question.kind == "destructive";
        let icon = if destructive {
            IconName::TriangleAlert
        } else {
            IconName::Send
        };
        let mut source = Vec::new();
        if let Some(scope) = question.task_scope() {
            source.extend([scope.task.clone(), scope.target.clone()]);
        } else if let Some((integration, tool)) = question.mcp_tool_scope() {
            source.extend([integration.to_owned(), tool.to_owned()]);
        } else if let Some(tool) = question.action.as_ref().and_then(|a| a.tool.clone()) {
            source.push(tool);
        }
        let mut card =
            super::super::action_card::card(icon, question.title.clone(), source.join(" · "))
                .when(destructive, |card| card.tag("Hard to undo"));
        if let Some(context) = &question.context {
            let short = preview(context);
            let shown = match (&short, expanded) {
                (Some(short), false) => short.clone(),
                _ => context.clone(),
            };
            card = card.child(super::super::action_card::body_text(shown));
            if short.is_some() {
                card = card.child(
                    Button::new(SharedString::from(format!("question-details-{id}")))
                        .focus_ring(false)
                        .ghost()
                        .xsmall()
                        .px_0()
                        .justify_start()
                        .cursor_pointer()
                        .accessibility_label(if expanded {
                            "Hide the rest of the command"
                        } else {
                            "Show the whole command"
                        })
                        .child(
                            h_flex()
                                .gap_1p5()
                                .text_color(theme.muted_foreground)
                                .child(
                                    Icon::new(if expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .size_3(),
                                )
                                .child(if expanded { "Show less" } else { "Show all" }),
                        )
                        .on_click(cx.listener(move |app, _, _, cx| {
                            app.toggle_question_details(id, cx);
                        })),
                );
            }
        }
        let mut grants: Vec<_> = question.choices.iter().filter(|c| c.id != DENY).collect();
        grants.sort_by_key(|choice| choice.id != APPROVE);
        let selected = submit
            .filter(|s| s.locks())
            .and_then(|s| s.answer.single())
            .or_else(|| view.approval_scopes.get(&id).map(String::as_str))
            .and_then(|chosen| grants.iter().find(|c| c.id == chosen))
            .or(grants.first())
            .copied();
        if grants.len() > 1 {
            let mut list = v_flex().w_full().gap_2();
            for choice in &grants {
                let on = selected.is_some_and(|s| s.id == choice.id);
                let (title, detail) = scope(choice, question, bot_name);
                let choice_id = choice.id.clone();
                list = list.child(
                    Radio::new(SharedString::from(format!(
                        "question-scope-{id}-{}",
                        choice.id
                    )))
                    .focus_ring(false)
                    .small()
                    .checked(on)
                    .disabled(locked)
                    .w_full()
                    .min_h(px(super::super::action_card::OPTION_HEIGHT))
                    .px_3()
                    .py_2()
                    .gap_2p5()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(if on {
                        theme.primary.opacity(0.5)
                    } else {
                        theme.border
                    })
                    .when(!locked, |row| row.cursor_pointer())
                    .when(on, |row| row.bg(theme.primary.opacity(0.08)))
                    .when(!locked && !on, |row| {
                        row.hover(|style| style.bg(theme.muted.opacity(0.5)))
                    })
                    .accessibility_label(if detail.is_empty() {
                        title.clone()
                    } else {
                        format!("{title}. {detail}")
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().text_sm().whitespace_normal().child(title))
                            .when(!detail.is_empty(), |column| column.child(note(detail))),
                    )
                    .on_click(cx.listener(move |app, _, _, cx| {
                        app.choose_approval_scope(id, choice_id.clone(), cx);
                    })),
                );
            }
            card = card.child(list);
        }
        let sending = |choice: &str| {
            submit
                .filter(|s| s.answer.contains(choice))
                .and_then(|s| match s.state {
                    SubmitState::Sending => Some("Sending…"),
                    SubmitState::Recorded => Some("Recorded"),
                    _ => None,
                })
        };
        let mut footer =
            super::super::action_card::footer().child(super::super::action_card::footer_note(
                &theme,
                if grants.len() > 1 {
                    "Sending a new message skips this."
                } else {
                    "Covers exactly this action, once. Sending a new message skips this."
                },
            ));
        if question.choices.iter().any(|c| c.id == DENY) {
            footer = footer.child(
                Button::new(SharedString::from(format!("question-{id}-{DENY}")))
                    .control(Tier::Secondary, Scale::Compact, locked, cx)
                    .label(sending(DENY).unwrap_or("Deny"))
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.answer_question(id, QuestionAnswer::Single(DENY.into()), window, cx)
                    })),
            );
        }
        if let Some(choice) = selected {
            let label = sending(&choice.id).unwrap_or(if choice.id == APPROVE {
                "Approve once"
            } else {
                "Always allow"
            });
            let choice_id = choice.id.clone();
            footer = footer.child(
                Button::new(SharedString::from(format!("question-{id}-approve")))
                    .control(Tier::Primary, Scale::Compact, locked, cx)
                    .label(label)
                    .accessibility_label(format!(
                        "{label}: {}",
                        scope(choice, question, bot_name).0
                    ))
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.answer_question(
                            id,
                            QuestionAnswer::Single(choice_id.clone()),
                            window,
                            cx,
                        )
                    })),
            );
        }
        if let Some(SubmitState::Failed(error)) = submit.map(|s| &s.state) {
            card = card.child(callout(error.clone(), &theme));
        }
        card.footer(footer).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::preview;

    #[test]
    fn long_context_previews_its_first_lines_on_a_char_boundary() {
        assert_eq!(preview("rm -rf ./dist"), None);
        let lines = (1..=8)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(preview(&lines).as_deref(), Some("1\n2\n3\n4\n5\n6"));
        let wide = "é".repeat(400);
        let short = preview(&wide).unwrap();
        assert!(short.len() <= 480 + '…'.len_utf8() && short.ends_with('…'));
    }
}
