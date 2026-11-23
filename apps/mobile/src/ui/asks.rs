//! Cards for what a bot waits on the owner to settle: a question's
//! choices, and a draft to send or discard. Each card reflects the server:
//! once settled, its turn's update removes it.

use std::collections::{BTreeMap, BTreeSet};

use else_wire::outgoing::{DraftContent, OutgoingDraft};
use else_wire::questions::{Question, QuestionKind};
use else_wire::secrets::SecretRequest;
use nsh_base::{alert_dialog, input};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::colors;
use super::session;
use crate::api::{ClientError, DraftEdit};
use crate::conversation::Ask;

/// A card's request to the server.
#[derive(Clone, PartialEq)]
enum Action {
    Idle,
    Sending,
    Failed(String),
    /// The response was lost: the server may have acted. Trying again is
    /// safe: a repeat of a settled ask is refused, an approval repeats as
    /// itself, and the server's own update removes the card once settled.
    Unknown,
}

pub fn card(bot: Uuid, ask: Ask) -> AnyElement {
    match ask {
        Ask::Question(question) => question_card(bot, *question).into_element(),
        Ask::Draft(draft) => draft_card(bot, *draft).into_element(),
        Ask::Secret(request) => secret_card(bot, *request).into_element(),
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
    dynamic(move || {
        let message = match action.get() {
            Action::Failed(message) => message,
            Action::Unknown => {
                "The connection dropped before the server answered. It may have gone through, and trying again is safe.".into()
            }
            Action::Idle | Action::Sending => return None,
        };
        Some(
            text(message)
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::destructive()),
        )
    })
}

/// The server's own words for a refusal (a settled question, a draft that
/// changed since it was shown); a lost response is an unknown outcome.
fn failed(err: ClientError) -> Action {
    if err.outcome_unknown() {
        return Action::Unknown;
    }
    match err {
        ClientError::Api { message, .. } => Action::Failed(message),
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
        session::spawn_held(async move {
            match client.answer_question(bot, id, choices, multiple).await {
                Ok(()) => action.try_set(Action::Idle),
                Err(err) => action.try_set(failed(err)),
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
                        let mark = if picked.with(|p| p.contains(&shown)) {
                            "✓ "
                        } else {
                            ""
                        };
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
                true,
                false,
                move || idle() && picked.with(|p| !p.is_empty()),
                move || send(picked.get_untracked().into_iter().collect()),
            )
        }))
        .child(failure(action))
}

/// Whether the draft card shows the draft or the owner's edits to it.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Reading,
    Editing,
}

fn draft_card(bot: Uuid, draft: OutgoingDraft) -> impl IntoElement {
    let mode = signal(Mode::Reading);
    let original = DraftEdit::of(&draft);
    let edit_subject = signal(original.subject.clone().unwrap_or_default());
    let edit_body = signal(original.body.clone());
    view().child(dynamic(move || match mode.get() {
        Mode::Reading => untracked(|| draft_view(bot, draft.clone(), mode)).into_element(),
        Mode::Editing => untracked(|| {
            draft_editor(
                bot,
                draft.id,
                original.clone(),
                edit_subject,
                edit_body,
                mode,
            )
        })
        .into_element(),
    }))
}

/// The draft's subject and body as editable fields. Save stores them on
/// the server at the version shown; the card then shows the new version.
fn draft_editor(
    bot: Uuid,
    draft_id: Uuid,
    original: DraftEdit,
    subject: Signal<String>,
    body: Signal<String>,
    mode: Signal<Mode>,
) -> impl IntoElement {
    let action = signal(Action::Idle);
    let idle = move || action.get() != Action::Sending;
    let has_subject = original.subject.is_some();
    let body_height = signal(120.0_f32);
    let save = {
        let original = original.clone();
        move || {
            let edit = DraftEdit {
                subject: original.subject.as_ref().map(|_| subject.get_untracked()),
                body: body.get_untracked(),
                ..original.clone()
            };
            if edit == original {
                mode.set(Mode::Reading);
                return;
            }
            action.set(Action::Sending);
            let client = session::server().client.clone();
            session::spawn_held(async move {
                match client.edit_draft(bot, draft_id, edit).await {
                    // The server's live update replaces this card with the
                    // saved version a moment later.
                    Ok(()) => action.try_set(Action::Idle) && mode.try_set(Mode::Reading),
                    Err(err) => action.try_set(failed(err)),
                }
            });
        }
    };
    let cancel = {
        let original = original.clone();
        move || {
            subject.set(original.subject.clone().unwrap_or_default());
            body.set(original.body.clone());
            action.set(Action::Idle);
            mode.set(Mode::Reading);
        }
    };
    let field = |label: &'static str| {
        text(label)
            .text_size(px(13.0))
            .text_color(colors::secondary())
    };
    frame()
        .child(eyebrow("EDITING DRAFT"))
        .child(has_subject.then(|| {
            view().gap_1().child(field("Subject")).child(
                input()
                    .min_h(px(40.0))
                    .px_3()
                    .rounded(px(10.0))
                    .bg(colors::fill())
                    .value(subject)
                    .on_change(move |t: &str| subject.set(t.to_owned()))
                    .text_size(px(15.0))
                    .text_color(colors::label())
                    .caret_color(|| color(colors::tint()))
                    .label("Subject"),
            )
        }))
        .child(
            view().gap_1().child(field("Message")).child(
                input()
                    .multiline(true)
                    .value(body)
                    .on_change(move |t: &str| body.set(t.to_owned()))
                    .on_content_size_change(move |size: &Size<Points>| {
                        body_height.set(size.height.0.clamp(80.0, 320.0));
                    })
                    .h(move || px(body_height.get() + 16.0))
                    .p_3()
                    .rounded(px(10.0))
                    .bg(colors::fill())
                    .text_size(px(15.0))
                    .line_height(px(20.0))
                    .text_color(colors::label())
                    .caret_color(|| color(colors::tint()))
                    .label("Message"),
            ),
        )
        .child(
            view()
                .flex_row()
                .gap_2()
                .child(button("Cancel", false, false, idle, cancel))
                .child(button(
                    move || {
                        SharedString::from(if action.get() == Action::Sending {
                            "Saving…"
                        } else {
                            "Save"
                        })
                    },
                    true,
                    false,
                    move || idle() && !body.with(|b| b.trim().is_empty()),
                    save,
                )),
        )
        .child(failure(action))
}

fn draft_view(bot: Uuid, draft: OutgoingDraft, mode: Signal<Mode>) -> impl IntoElement {
    let action = signal(Action::Idle);
    let idle = move || action.get() != Action::Sending;
    // Everything Send would deliver is on the card: hidden copies and files
    // included.
    let (cc, bcc) = match &draft.content {
        DraftContent::Email { cc, bcc, .. } => (cc.join(", "), bcc.join(", ")),
        _ => (String::new(), String::new()),
    };
    let files: Vec<String> = draft
        .content
        .attachments()
        .iter()
        .map(|file| file.name.clone())
        .collect();
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
        session::spawn_held(async move {
            let choice = alert_dialog()
                .title("Send it now?")
                .description(format!("It goes to {to} exactly as shown."))
                .cancel("cancel", "Cancel")
                .action("send", "Send")
                .show()
                .await;
            if !matches!(choice, Ok(r) if r.action.as_deref() == Some("send")) {
                return false;
            }
            action.try_set(Action::Sending);
            let client = session::server().client.clone();
            match client.approve_draft(bot, &draft).await {
                Ok(()) => action.try_set(Action::Idle),
                Err(err) => action.try_set(failed(err)),
            }
        });
    };
    let discard = move || {
        action.set(Action::Sending);
        let client = session::server().client.clone();
        session::spawn_held(async move {
            match client.discard_draft(bot, draft_id).await {
                Ok(()) => action.try_set(Action::Idle),
                Err(err) => action.try_set(failed(err)),
            }
        });
    };

    frame()
        .child(
            view()
                .flex_row()
                .items_center()
                .justify_between()
                .child(eyebrow("DRAFT TO REVIEW"))
                .child(
                    text("Edit")
                        .text_size(px(15.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors::tint())
                        .opacity(move || if idle() { 1.0_f32 } else { 0.45 })
                        .px_2()
                        .py_1()
                        .active(|s| s.opacity(0.5))
                        .role(AccessibilityRole::Button)
                        .label("Edit draft")
                        .on_press(move || {
                            if idle() {
                                mode.set(Mode::Editing);
                            }
                        }),
                ),
        )
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
        .child((!cc.is_empty()).then(|| {
            text(format!("Cc {cc}"))
                .text_size(px(15.0))
                .text_color(colors::secondary())
        }))
        .child((!bcc.is_empty()).then(|| {
            text(format!("Bcc {bcc}"))
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
        .child((!files.is_empty()).then(|| {
            text(format!("Attached: {}", files.join(", ")))
                .text_size(px(15.0))
                .text_color(colors::label())
        }))
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

/// A bot's request for credentials: one masked field per variable it
/// named. Save stores them in the server's vault, where every bot may use
/// them; the card never shows a stored value again.
fn secret_card(bot: Uuid, request: SecretRequest) -> impl IntoElement {
    let action = signal(Action::Idle);
    let idle = move || action.get() != Action::Sending;
    let values = signal(BTreeMap::<String, String>::new());
    let names = request.env.clone();
    let complete = {
        let names = names.clone();
        computed(move || {
            values.with(|v| {
                names
                    .iter()
                    .all(|n| v.get(n).is_some_and(|s| !s.is_empty()))
            })
        })
    };
    let id = request.id;
    let answer = move |env: Option<BTreeMap<String, String>>| {
        action.set(Action::Sending);
        // The values leave the phone's memory with the request: a lost
        // response never leaves them in the fields.
        values.set(BTreeMap::new());
        let client = session::server().client.clone();
        session::spawn_held(async move {
            match client.answer_secret(bot, id, env).await {
                Ok(()) => action.try_set(Action::Idle),
                Err(err) => action.try_set(failed(err)),
            }
        });
    };
    let fields: Vec<AnyElement> = names
        .iter()
        .map(|name| {
            let key = name.clone();
            let shown = name.clone();
            view()
                .gap_1()
                .child(
                    text(name.clone())
                        .text_size(px(13.0))
                        .font_family("Menlo")
                        .text_color(colors::secondary()),
                )
                .child(
                    input()
                        .min_h(px(40.0))
                        .px_3()
                        .rounded(px(10.0))
                        .bg(colors::fill())
                        .value(move || values.with(|v| v.get(&shown).cloned().unwrap_or_default()))
                        .on_change(move |t: &str| {
                            values.update(|v| {
                                v.insert(key.clone(), t.to_owned());
                            })
                        })
                        .masked(true)
                        .auto_capitalize(AutoCapitalize::None)
                        .auto_correct(false)
                        .text_size(px(15.0))
                        .text_color(colors::label())
                        .caret_color(|| color(colors::tint()))
                        .label(name.clone()),
                )
                .into_element()
        })
        .collect();
    let detail = request.description.clone().filter(|d| !d.trim().is_empty());
    frame()
        .child(eyebrow("NEEDS A SECRET"))
        .child(
            text(request.name.clone())
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label()),
        )
        .child(
            text(request.reason.clone())
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(colors::label()),
        )
        .child(detail.map(|detail| {
            text(detail)
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(colors::secondary())
        }))
        .child(request.website.clone().map(|site| {
            text(format!("For {site}"))
                .text_size(px(15.0))
                .text_color(colors::secondary())
        }))
        .child(view().gap_3().children(fields))
        .child(
            text("Saved in the server's vault, where every bot can use it.")
                .text_size(px(13.0))
                .text_color(colors::secondary()),
        )
        .child(
            view()
                .flex_row()
                .gap_2()
                .child(button("Decline", false, true, idle, move || answer(None)))
                .child(button(
                    "Save",
                    true,
                    false,
                    move || idle() && complete.get(),
                    move || answer(Some(values.get_untracked())),
                )),
        )
        .child(failure(action))
}
