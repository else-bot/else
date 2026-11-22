//! Cards for what a bot waits on the owner to settle: a question's
//! choices, and a draft to send or discard. Each card reflects the server:
//! once settled, its turn's update removes it.

use std::collections::BTreeSet;

use else_wire::outgoing::OutgoingDraft;
use else_wire::questions::{Question, QuestionKind};
use nsh_base::alert_dialog;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::colors;
use super::session;
use crate::api::ClientError;
use crate::conversation::Ask;

/// A card's request to the server.
#[derive(Clone, PartialEq)]
enum Action {
    Idle,
    Sending,
    Failed(String),
}

pub fn card(bot: Uuid, ask: Ask) -> AnyElement {
    match ask {
        Ask::Question(question) => question_card(bot, *question).into_element(),
        Ask::Draft(draft) => draft_card(bot, *draft).into_element(),
    }
}

fn frame() -> View {
    view()
        .mx_4()
        .my_2()
        .p_4()
        .gap_3()
        .rounded(px(16.0))
        .border_curve(BorderCurve::Continuous)
        .bg(colors::card())
        .border_1()
        .border_color(colors::separator())
}

fn eyebrow(label: &'static str) -> impl IntoElement {
    text(label)
        .text_size(px(13.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(colors::warning())
}

fn failure(action: Signal<Action>) -> impl IntoElement {
    dynamic(move || match action.get() {
        Action::Failed(message) => Some(
            text(message)
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::destructive()),
        ),
        _ => None,
    })
}

fn failed(err: ClientError) -> Action {
    match err {
        ClientError::Api { status: 409, .. } => {
            Action::Failed("This was already settled elsewhere.".into())
        }
        err => Action::Failed(err.to_string()),
    }
}

/// A full-width button: filled for the main action, tinted otherwise.
fn button(
    label: impl IntoReactive<SharedString>,
    filled: bool,
    destructive: bool,
    enabled: impl Fn() -> bool + Copy + 'static,
    on_press: impl Fn() + 'static,
) -> View {
    let accent = move || {
        if destructive {
            colors::destructive()
        } else {
            colors::tint()
        }
    };
    view()
        .flex_1()
        .min_h(px(44.0))
        .px_3()
        .rounded(px(12.0))
        .border_curve(BorderCurve::Continuous)
        .items_center()
        .justify_center()
        .bg(move || if filled { accent() } else { colors::fill() })
        .opacity(move || if enabled() { 1.0_f32 } else { 0.45 })
        .active(|s| s.opacity(0.7))
        .role(AccessibilityRole::Button)
        .on_press(move || {
            if enabled() {
                on_press();
            }
        })
        .child(
            text(label)
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_center()
                .text_color(move || if filled { colors::on_tint() } else { accent() }),
        )
}

fn question_card(bot: Uuid, question: Question) -> impl IntoElement {
    let action = signal(Action::Idle);
    let picked = signal(BTreeSet::<String>::new());
    let idle = move || action.get() != Action::Sending;
    let id = question.id;
    let multiple = question.multiple;
    let send = move |choices: Vec<String>| {
        action.set(Action::Sending);
        let client = session::server().client.clone();
        spawn(async move {
            match client.answer_question(bot, id, choices, multiple).await {
                Ok(()) => action.set(Action::Idle),
                Err(err) => action.set(failed(err)),
            }
        });
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
                        let mark = if picked.with(|p| p.contains(&shown)) { "✓ " } else { "" };
                        SharedString::from(format!("{mark}{label}"))
                    },
                    false,
                    false,
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
                approval && index == 0,
                approval && index == 0 && question.kind == QuestionKind::Destructive,
                idle,
                move || send(vec![choice_id.clone()]),
            )
            .into_element()
        })
        .collect();
    frame()
        .child(eyebrow(if approval { "NEEDS YOUR APPROVAL" } else { "QUESTION" }))
        .child(
            text(question.title.clone())
                .text_size(px(17.0))
                .line_height(px(22.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label()),
        )
        .child(question.context.clone().filter(|c| !c.trim().is_empty()).map(|context| {
            text(context)
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(colors::secondary())
        }))
        .child(view().gap_2().children(choices))
        .child(multiple.then(|| {
            button(
                "Submit",
                true,
                false,
                move || idle() && picked.with(|p| !p.is_empty()),
                move || send(picked.get_untracked().into_iter().collect()),
            )
        }))
        .child(failure(action))
}

fn draft_card(bot: Uuid, draft: OutgoingDraft) -> impl IntoElement {
    let action = signal(Action::Idle);
    let idle = move || action.get() != Action::Sending;
    let recipient = draft.content.recipient();
    let title = draft.content.title();
    // An email's title is its subject, so it is not repeated below.
    let subject = draft
        .content
        .subject()
        .map(str::to_owned)
        .filter(|s| !s.is_empty() && *s != title);
    let body = draft.content.body().to_owned();
    let draft_id = draft.id;
    let confirm_to = recipient.clone().unwrap_or_else(|| "its recipient".into());
    let can_approve = draft.can_approve;

    let approve = move || {
        let draft = draft.clone();
        let to = confirm_to.clone();
        spawn(async move {
            let choice = alert_dialog()
                .title("Send it now?")
                .description(format!("It goes to {to} exactly as shown."))
                .cancel("cancel", "Cancel")
                .action("send", "Send")
                .show()
                .await;
            if !matches!(choice, Ok(r) if r.action.as_deref() == Some("send")) {
                return;
            }
            action.set(Action::Sending);
            let client = session::server().client.clone();
            match client.approve_draft(bot, &draft).await {
                Ok(()) => action.set(Action::Idle),
                Err(err) => action.set(failed(err)),
            }
        });
    };
    let discard = move || {
        action.set(Action::Sending);
        let client = session::server().client.clone();
        spawn(async move {
            match client.discard_draft(bot, draft_id).await {
                Ok(()) => action.set(Action::Idle),
                Err(err) => action.set(failed(err)),
            }
        });
    };

    frame()
        .child(eyebrow("DRAFT TO REVIEW"))
        .child(
            text(title)
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label()),
        )
        .child(recipient.map(|to| {
            text(format!("To {to}"))
                .text_size(px(15.0))
                .text_color(colors::secondary())
        }))
        .child(subject.map(|subject| {
            text(subject)
                .text_size(px(15.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label())
        }))
        .child(
            view().p_3().rounded(px(10.0)).bg(colors::fill()).child(
                text(body)
                    .selectable(true)
                    .text_size(px(15.0))
                    .line_height(px(20.0))
                    .text_color(colors::label()),
            ),
        )
        .child(
            view()
                .flex_row()
                .gap_2()
                .child(button("Discard", false, true, idle, discard))
                .child(button(
                    "Send",
                    true,
                    false,
                    move || idle() && can_approve,
                    approve,
                )),
        )
        .child((!can_approve).then(|| {
            text("The server cannot send this draft as it is. Open it in the desktop app.")
                .text_size(px(13.0))
                .text_color(colors::secondary())
        }))
        .child(failure(action))
}
