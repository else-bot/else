//! An approval card: the action a bot wants to take, the exact command or
//! request, and how long the approval lasts. The owner picks one scope from
//! the choices the server offered, so the main button says exactly what
//! they agree to.

use super::*;
use crate::app::clip;
use crate::app::transcript::CardKey;
use crate::controls::{Scale, Tier};
use crate::model::QuestionChoice;
use crate::model::QuestionKind;
use crate::model::QuestionStatus;
use gpui_kit::component::radio::Radio;

/// How long an approval choice lasts, as the scope list shows it.
fn scope(choice: &QuestionChoice, question: &Question, bot: &str) -> (String, String) {
    match choice.id.as_str() {
        APPROVE => (
            "Just this once".into(),
            "Covers exactly this action.".into(),
        ),
        APPROVE_TASK
            if question.action.as_ref().and_then(|a| a.tool.as_deref())
                == Some("computer_resize") =>
        {
            let up_to = question
                .task_scope()
                .map_or_else(|| "this size".to_owned(), |scope| scope.target.clone());
            (
                format!("Always allow {up_to}"),
                format!("{bot} can raise its computer's memory {up_to} without asking. It still asks before re-running a job."),
            )
        }
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
                || match question.action.as_ref().and_then(|a| a.tool.as_deref()) {
                    Some(tool) => format!("Always allow {tool}"),
                    None => "Always allow this tool".to_owned(),
                },
                |(integration, tool)| format!("Always allow {integration} · {tool}"),
            ),
            format!(
                "For {} actions with this tool in {bot}'s chats, routine runs and tasks.",
                question.kind.as_str()
            ),
        ),
        APPROVE_ROUTINE if question.kind == QuestionKind::External => (
            "Always for this routine".into(),
            "Allows external computer and host actions for its saved task. Changing the task clears this.".into(),
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

/// The most of a context shown before "Show all".
const PREVIEW_LINES: usize = 6;
const PREVIEW_CHARS: usize = 480;

/// The first lines of a long context, or `None` when it fits whole or the
/// rest is too little to hide. A line cut partway ends between words with an
/// ellipsis; "Show all" marks the hidden lines.
pub(super) fn preview(context: &str) -> Option<String> {
    let lines = context
        .match_indices('\n')
        .nth(PREVIEW_LINES - 1)
        .map_or(context.len(), |(ix, _)| ix);
    let end = clip::char_end(&context[..lines], PREVIEW_CHARS);
    if !clip::hides_enough(&context[end..], PREVIEW_CHARS) {
        return None;
    }
    let words = clip::word_end(context, end);
    let mid_line = !context[words..]
        .trim_start_matches([' ', '\t'])
        .starts_with(['\n', '\r']);
    let close = if mid_line { "…" } else { "" };
    Some(format!("{}{close}", &context[..words]))
}

impl ElseApp {
    fn choose_approval_scope(&mut self, question: Uuid, choice: String, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if view.cards.submit(question).is_some_and(QuestionForm::locks) {
            return;
        }
        view.cards.question_mut(question).scope = Some(choice);
        self.remeasure_row(RowKey::Card(CardKey::Question(question)), cx);
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
        let expanded = view
            .cards
            .questions
            .get(&id)
            .is_some_and(|card| card.expanded);
        let note = |text: String| {
            div()
                .text_xs()
                .whitespace_normal()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        if !question.is_pending() {
            let approved = question.status == QuestionStatus::Answered
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
                    div().w_full().px_4().pb_4().child(
                        self.formatted_context(
                            id,
                            Extent::Whole,
                            theme.foreground,
                            theme.background,
                            cx,
                        )
                        .unwrap_or_else(|| {
                            super::super::action_card::body_text(context.clone()).into_any_element()
                        }),
                    ),
                );
            }
            return card.into_any_element();
        }
        let locked =
            submit.is_some_and(QuestionForm::locks) || self.connection != Connection::Connected;
        let destructive = question.kind == QuestionKind::Destructive;
        let attachments = question.mail_attachments();
        let icon = if destructive {
            IconName::TriangleAlert
        } else if attachments.is_some() {
            IconName::FileText
        } else {
            IconName::Send
        };
        let deletion = question.bot_deletion();
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
        if let Some(deletion) = deletion {
            let detail = [deletion.model.as_deref(), deletion.category.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            card = card.child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2p5()
                    .child(
                        crate::avatar::bot_avatar(
                            SharedString::from(format!("question-deletion-avatar-{id}")),
                            deletion.avatar,
                            px(28.),
                        )
                        .still(),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(deletion.name.clone()),
                            )
                            .when(!detail.is_empty(), |column| column.child(note(detail))),
                    ),
            );
        }
        if let Some(mail) = attachments {
            let mut origin = mail.address.clone();
            if let Some(subject) = &mail.subject {
                origin = format!("{subject} · {origin}");
            }
            if !mail.from.is_empty() {
                origin = format!("{origin} · from {}", mail.from.join(", "));
            }
            let mut files = v_flex().w_full().gap_1p5().child(note(origin));
            for file in &mail.files {
                let detail = format!(
                    "{} · {}",
                    file.kind(),
                    crate::attachments::byte_size(file.size)
                );
                files = files.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2p5()
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .child(
                            Icon::new(IconName::FileText)
                                .size_4()
                                .text_color(theme.muted_foreground),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .truncate()
                                .child(file.name.clone()),
                        )
                        .child(note(detail)),
                );
            }
            card = card.child(files);
        }
        if let Some(context) = question.context.as_ref().filter(|_| attachments.is_none()) {
            let short = preview(context);
            let (shown, extent) = match (&short, expanded) {
                (Some(short), false) => (short.clone(), Extent::Preview),
                _ => (context.clone(), Extent::Whole),
            };
            card = card.child(
                self.formatted_context(id, extent, theme.foreground, theme.secondary, cx)
                    .unwrap_or_else(|| {
                        super::super::action_card::body_text(shown).into_any_element()
                    }),
            );
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
            .or_else(|| view.cards.questions.get(&id)?.scope.as_deref())
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
                    Request::Sending { .. } => Some("Sending…"),
                    Request::Accepted(()) => Some("Recorded"),
                    _ => None,
                })
        };
        let mut footer = super::super::action_card::footer().note(
            match selected.map(|choice| choice.id.as_str()) {
                _ if deletion.is_some() => {
                    "Deletes only this bot, once. Sending a new message skips this."
                }
                _ if attachments.is_some() => {
                    "Lets the bot read and search only these files. Sending a new message skips this."
                }
                Some(APPROVE) | None => {
                    "Covers exactly this action, once. Sending a new message skips this."
                }
                Some(APPROVE_ROUTINE) => {
                    "Covers this routine until its task changes. Replying \"Always allow\" also chooses this."
                }
                Some(APPROVE_FOREVER) => {
                    "Covers destructive actions from now on."
                }
                Some(_) => {
                    "Covers this tool or task and effect from now on. Replying \"Always allow\" also chooses this."
                }
            },
        );
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
            let label: SharedString = match (sending(&choice.id), deletion) {
                (Some(state), _) => state.into(),
                (None, Some(deletion)) => format!("Delete {}", deletion.name).into(),
                (None, None) if choice.id == APPROVE => "Approve once".into(),
                (None, None) => "Always allow".into(),
            };
            let choice_id = choice.id.clone();
            footer = footer.child(
                Button::new(SharedString::from(format!("question-{id}-approve")))
                    .control(Tier::Primary, Scale::Compact, locked, cx)
                    .label(label.clone())
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
        if let Some(error) = submit.and_then(|s| s.state.error()) {
            card = card.child(callout(error.to_owned(), &theme));
        }
        card.footer(footer).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::preview;

    #[test]
    fn a_context_that_fits_or_barely_overflows_shows_whole() {
        assert_eq!(preview("rm -rf ./dist"), None);
        let over = format!("{}done.", "word ".repeat(96));
        assert_eq!(preview(&over), None, "a few characters over the limit");
        let one_word = format!("Run {}", "é".repeat(477));
        assert_eq!(preview(&one_word), None, "one character over the limit");
        let fenced = "```sh\npwd\nls\ndate\nwhoami\nid\n```\n.";
        assert_eq!(preview(fenced), None, "a closing fence draws nothing");
        let seven = "1\n2\n3\n4\n5\n6\nlast line";
        assert_eq!(preview(seven), None, "one short line over the limit");
    }

    #[test]
    fn a_long_context_collapses_between_words() {
        let context = "alpha beta ".repeat(80);
        let short = preview(&context).unwrap();
        let shown = short.strip_suffix('…').expect("a mid-line cut ends in …");
        assert!(shown.chars().count() <= 480, "{shown}");
        assert!(
            context[shown.len()..].starts_with(' '),
            "cut between words: {short}"
        );
        let wide = "é".repeat(800);
        let short = preview(&wide).unwrap();
        assert_eq!(short.chars().count(), 481, "one word is cut at the limit");
        let code = format!("```sh\n{}xx\n```\n{}", "é ".repeat(234), "word ".repeat(40));
        assert!(
            preview(&code).unwrap().ends_with("xx\n```"),
            "a cut at a line end adds no ellipsis to the code"
        );
    }

    #[test]
    fn many_lines_collapse_to_the_first_six() {
        let lines = (1..=8)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(preview(&lines).as_deref(), Some("1\n2\n3\n4\n5\n6"));
        let tail = format!("1\n2\n3\n4\n5\n6\n{}", "word ".repeat(40));
        assert_eq!(preview(&tail).as_deref(), Some("1\n2\n3\n4\n5\n6"));
    }
}
