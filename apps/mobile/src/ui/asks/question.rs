//! A question's choices: one tap answers, or several picks and Submit.

use std::collections::BTreeSet;

use else_wire::questions::{Question, QuestionAnswer, QuestionKind};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::super::parts::colors;
use super::super::session;
use super::{Fill, button, eyebrow, failure, frame};
use else_client::request::Request;

pub(super) fn card(bot: Uuid, question: Question) -> impl IntoElement {
    let request = signal(Request::<QuestionAnswer>::Idle);
    let picked = signal(BTreeSet::<String>::new());
    let idle = move || !request.with(Request::busy);
    let id = question.id;
    let multiple = question.multiple;
    let send = move |choices: Vec<String>| {
        let answer = if multiple {
            QuestionAnswer::Multiple(choices)
        } else {
            match choices.into_iter().next() {
                Some(choice) => QuestionAnswer::Single(choice),
                None => return,
            }
        };
        let client = session::server().client.clone();
        session::submit(
            request,
            answer,
            move |answer| client.answer_question(bot, id, answer),
            |()| {},
        );
    };
    let approval = question.kind != QuestionKind::Choice;
    let choices: Vec<AnyElement> = question
        .choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let choice_id = choice.id.clone();
            if multiple {
                let shown = choice_id.clone();
                let label = choice.label.clone();
                return button(
                    move || {
                        let mark = if picked.with(|p| p.contains(&shown)) {
                            "✓ "
                        } else {
                            ""
                        };
                        SharedString::from(format!("{mark}{label}"))
                    },
                    Fill::Plain,
                    idle,
                    move || {
                        picked.update(|p| {
                            if !p.remove(&choice_id) {
                                p.insert(choice_id.clone());
                            }
                        })
                    },
                )
                .into_element();
            }
            button(
                SharedString::from(choice.label.clone()),
                match (
                    approval && index == 0,
                    question.kind == QuestionKind::Destructive,
                ) {
                    (true, true) => Fill::ProminentDestructive,
                    (true, false) => Fill::Prominent,
                    (false, _) => Fill::Plain,
                },
                idle,
                move || send(vec![choice_id.clone()]),
            )
            .into_element()
        })
        .collect();
    frame()
        .child(eyebrow(if approval {
            "NEEDS YOUR APPROVAL"
        } else {
            "QUESTION"
        }))
        .child(
            text(question.title.clone())
                .text_size(px(17.0))
                .line_height(px(22.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label()),
        )
        .child(
            question
                .context
                .clone()
                .filter(|c| !c.trim().is_empty())
                .map(|context| {
                    text(context)
                        .text_size(px(15.0))
                        .line_height(px(20.0))
                        .text_color(colors::secondary())
                }),
        )
        .child(view().gap_2().children(choices))
        .child(multiple.then(|| {
            button(
                "Submit",
                Fill::Prominent,
                move || idle() && picked.with(|p| !p.is_empty()),
                move || send(picked.get_untracked().into_iter().collect()),
            )
        }))
        .child(failure(request))
}
