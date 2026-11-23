//! Cards for what a bot waits on the owner to settle: a question's
//! choices, and a draft to send or discard. Each card reflects the server:
//! once settled, its turn's update removes it.

use std::collections::{BTreeMap, BTreeSet};

use else_wire::notices::{OwnerNotice, OwnerNoticeKind};
use else_wire::outgoing::{DraftAttachment, DraftContent, DraftEdit, OutgoingDraft};
use else_wire::questions::{Question, QuestionKind};
use else_wire::secrets::SecretRequest;
use nsh_base::{alert_dialog, input};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::colors;
use super::session;
use else_client::api::{SaveSecret, validate_secret_metadata};
use else_wire::questions::QuestionAnswer;

use else_client::request::Request;

use crate::api::Message;
use crate::conversation::Ask;

/// What a draft card asks of the server.
#[derive(Debug, Clone, PartialEq)]
enum DraftOp {
    Approve(DraftEdit),
    /// The turn that drafted it has ended: a reply carrying it asks the bot
    /// to send it.
    SendAsIs(Message, Uuid),
    Discard,
}

pub fn card(bot: Uuid, ask: Ask) -> AnyElement {
    match ask {
        Ask::Question(question) => question_card(bot, *question).into_element(),
        Ask::Draft(draft) => draft_card(bot, *draft).into_element(),
        Ask::Secret(request) => secret_card(bot, *request).into_element(),
        Ask::Notice(notice) => notice_card(*notice).into_element(),
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

/// Why the card's last request did not go through. A lost response is an
/// unknown outcome: trying again repeats it, a repeat of a settled ask is
/// refused, and the server's own update removes the card once settled.
fn failure<Op: Clone + 'static>(request: Signal<Request<Op>>) -> impl IntoElement {
    dynamic(move || {
        let message = request.with(|r| match r {
            Request::Unconfirmed { error, .. } => Some(format!(
                "{error} It may have gone through; trying again is safe."
            )),
            r => r.error().map(str::to_owned),
        })?;
        Some(
            text(message)
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::destructive()),
        )
    })
}

/// How a card's button looks: filled for the main action, plain for the
/// others, in red for one that cannot be undone.
#[derive(Clone, Copy, PartialEq)]
enum Fill {
    Prominent,
    ProminentDestructive,
    Plain,
    PlainDestructive,
}

/// A full-width button. Disabled, it takes no presses and screen readers
/// say it is dimmed.
fn button(
    label: impl IntoReactive<SharedString>,
    fill: Fill,
    enabled: impl Fn() -> bool + Copy + 'static,
    on_press: impl Fn() + 'static,
) -> View {
    let accent = match fill {
        Fill::Prominent | Fill::Plain => colors::tint(),
        Fill::ProminentDestructive | Fill::PlainDestructive => colors::destructive(),
    };
    let filled = matches!(fill, Fill::Prominent | Fill::ProminentDestructive);
    view()
        .flex_1()
        .min_h(px(44.0))
        .px_3()
        .rounded(px(12.0))
        .border_curve(BorderCurve::Continuous)
        .items_center()
        .justify_center()
        .bg(if filled { accent } else { colors::fill() })
        .disabled(move || !enabled())
        .disabled_style(|s| s.opacity(0.45))
        .active(|s| s.opacity(0.7))
        .role(AccessibilityRole::Button)
        .on_press(on_press)
        .child(
            text(label)
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_center()
                .text_color(if filled { colors::on_tint() } else { accent }),
        )
}

fn question_card(bot: Uuid, question: Question) -> impl IntoElement {
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

/// Whether the draft card shows the draft or the owner's edits to it.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Reading,
    Editing,
}

fn draft_card(bot: Uuid, draft: OutgoingDraft) -> impl IntoElement {
    let mode = signal(Mode::Reading);
    let original = draft.edit();
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
    let request = signal(Request::<DraftEdit>::Idle);
    let idle = move || !request.with(Request::busy);
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
            let client = session::server().client.clone();
            // The server's live update replaces this card with the saved
            // version a moment later.
            session::submit(
                request,
                edit,
                move |edit| client.edit_draft(bot, draft_id, edit),
                move |_| {
                    request.try_set(Request::Idle);
                    mode.try_set(Mode::Reading);
                },
            );
        }
    };
    let cancel = {
        let original = original.clone();
        move || {
            subject.set(original.subject.clone().unwrap_or_default());
            body.set(original.body.clone());
            request.set(Request::Idle);
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
                .child(button("Cancel", Fill::Plain, idle, cancel))
                .child(button(
                    move || {
                        SharedString::from(if request.with(Request::busy) {
                            "Saving…"
                        } else {
                            "Save"
                        })
                    },
                    Fill::Prominent,
                    move || idle() && !body.with(|b| b.trim().is_empty()),
                    save,
                )),
        )
        .child(failure(request))
}

fn draft_view(bot: Uuid, draft: OutgoingDraft, mode: Signal<Mode>) -> impl IntoElement {
    let request = signal(Request::<DraftOp>::Idle);
    let idle = move || !request.with(Request::busy);
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
            let op = if can_approve {
                DraftOp::Approve(draft.edit())
            } else {
                // The turn that drafted it has ended, so the button cannot
                // approve it. A reply carrying the draft asks the bot to
                // send it, as the desktop's "Reply in chat" does. One ID
                // per version: a repeat or a retry posts it once.
                let reply = Message {
                    text: "Send it as it is.".into(),
                    files: Vec::new(),
                    draft: Some(DraftAttachment {
                        id: draft.id,
                        edit: draft.edit(),
                    }),
                    answers_notice: None,
                };
                let id = format!("send-as-is:{}", draft.version);
                DraftOp::SendAsIs(reply, Uuid::new_v5(&draft.id, id.as_bytes()))
            };
            submit_draft(bot, draft.id, request, op);
            true
        });
    };
    let discard = move || submit_draft(bot, draft_id, request, DraftOp::Discard);

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
                .child(button("Discard", Fill::PlainDestructive, idle, discard))
                .child(button("Send", Fill::Prominent, idle, approve)),
        )
        .child((!can_approve).then(|| {
            text("The bot has finished its turn, so Send asks it in the chat to send this draft as it is.")
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::secondary())
        }))
        .child(failure(request))
}

/// Sends a draft card's request. Once accepted, the server's update
/// replaces the card with its receipt.
fn submit_draft(bot: Uuid, draft: Uuid, request: Signal<Request<DraftOp>>, op: DraftOp) {
    let client = session::server().client.clone();
    session::submit(
        request,
        op,
        move |op| async move {
            match op {
                DraftOp::Approve(edit) => client
                    .approve_draft(bot, draft, edit, false)
                    .await
                    .map(drop),
                DraftOp::SendAsIs(reply, request_id) => {
                    client.send(bot, reply.request(request_id)).await.map(drop)
                }
                DraftOp::Discard => client.discard_draft(bot, draft).await.map(drop),
            }
        },
        |()| {},
    );
}

/// A bot's request for credentials: one masked field per variable it
/// named. Save stores them in the server's vault, where every bot may use
/// them; the card never shows a stored value again.
fn secret_card(bot: Uuid, asked: SecretRequest) -> impl IntoElement {
    let request = signal(Request::<Option<SaveSecret>>::Idle);
    let idle = move || !request.with(Request::busy);
    let values = signal(BTreeMap::<String, String>::new());
    let names = asked.env.clone();
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
    let id = asked.id;
    // Saved with the description and website the bot gave, as the desktop
    // saves it; the phone does not edit them.
    let meta = stored((
        asked.description.clone().unwrap_or_default(),
        asked.website.clone().unwrap_or_default(),
    ));
    let answer = move |env: Option<BTreeMap<String, String>>| {
        let (description, website) = meta.get();
        let secret = match env.map(|env| {
            validate_secret_metadata(&description, &website).map(|website| SaveSecret {
                env,
                description: description.clone(),
                website,
            })
        }) {
            Some(Ok(secret)) => Some(secret),
            Some(Err(message)) => {
                request.update(|r| r.reject(message));
                return;
            }
            None => None,
        };
        // The values leave the fields with the request: a lost response
        // never leaves them on screen.
        values.set(BTreeMap::new());
        let client = session::server().client.clone();
        // Once accepted, the card keeps no copy of the values while it waits
        // for the server's update to remove it.
        session::submit(
            request,
            secret,
            move |secret| client.answer_secret(bot, id, secret),
            move |()| {
                request.try_set(Request::Accepted(None));
            },
        );
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
    let detail = asked.description.clone().filter(|d| !d.trim().is_empty());
    frame()
        .child(eyebrow("NEEDS A SECRET"))
        .child(
            text(asked.name.clone())
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label()),
        )
        .child(
            text(asked.reason.clone())
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
        .child(asked.website.clone().map(|site| {
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
                .child(button("Decline", Fill::PlainDestructive, idle, move || {
                    answer(None)
                }))
                .child(button(
                    "Save",
                    Fill::Prominent,
                    move || idle() && complete.get(),
                    move || answer(Some(values.get_untracked())),
                )),
        )
        .child(failure(request))
}

/// What a notice card asks of the server.
#[derive(Debug, Clone, PartialEq)]
enum NoticeOp {
    Dismiss,
    /// A reply quoting the ask, under a request ID fixed by its text.
    Reply(Message, Uuid),
}

/// The bot is blocked or needs a decision: its ask in its own words, what
/// it tried, and the owner's two ways to settle it. A reply goes to the bot
/// that asked, quoting the ask, and settles only this notice where the
/// server can.
fn notice_card(notice: OwnerNotice) -> impl IntoElement {
    let request = signal(Request::<NoticeOp>::Idle);
    let idle = move || !request.with(Request::busy);
    let replying = signal(false);
    let body = signal(String::new());
    let body_height = signal(60.0_f32);
    let bot = notice.bot_id;
    let id = notice.id;
    let summary = notice.summary.clone();
    let send = move |op: NoticeOp| {
        let client = session::server().client.clone();
        session::submit(
            request,
            op,
            move |op| async move {
                let result = match op {
                    NoticeOp::Dismiss => client.dismiss_owner_notice(bot, id).await.map(drop),
                    NoticeOp::Reply(mut message, request_id) => {
                        // Servers that cannot settle one ask refuse the
                        // field; the quote still tells the bot which ask.
                        if !client.clone().capabilities().await?.answers_notice {
                            message.answers_notice = None;
                        }
                        client
                            .send(bot, message.request(request_id))
                            .await
                            .map(drop)
                    }
                };
                match result {
                    // The owner's message settled it first; its update
                    // removes the card.
                    Err(err) if err.code() == Some("owner_notice_settled") => Ok(()),
                    result => result,
                }
            },
            move |()| {
                body.try_set(String::new());
            },
        );
    };
    let reply = move || {
        let words = body.get_untracked().trim().to_owned();
        let text = else_client::reply::replying_to("your earlier message", &summary, &words);
        let request_id = Uuid::new_v5(&id, text.as_bytes());
        send(NoticeOp::Reply(
            Message {
                text,
                files: Vec::new(),
                draft: None,
                answers_notice: Some(id),
            },
            request_id,
        ));
    };
    let eyebrow_label = match notice.kind {
        OwnerNoticeKind::Blocked => "BLOCKED, NEEDS YOU",
        OwnerNoticeKind::Decision => "NEEDS A DECISION",
        OwnerNoticeKind::Other(_) => "NEEDS YOU",
    };
    frame()
        .child(eyebrow(eyebrow_label))
        .child(
            text(notice.summary.clone())
                .selectable(true)
                .text_size(px(17.0))
                .line_height(px(22.0))
                .text_color(colors::label()),
        )
        .child(
            notice
                .tried
                .clone()
                .filter(|t| !t.trim().is_empty())
                .map(|tried| {
                    text(format!("Tried: {tried}"))
                        .text_size(px(15.0))
                        .line_height(px(20.0))
                        .text_color(colors::secondary())
                }),
        )
        .child(dynamic(move || {
            if !replying.get() {
                return view()
                    .flex_row()
                    .gap_2()
                    .child(button("Mark handled", Fill::Plain, idle, move || {
                        send(NoticeOp::Dismiss)
                    }))
                    .child(button("Reply", Fill::Prominent, idle, move || {
                        replying.set(true)
                    }))
                    .into_element();
            }
            untracked(|| {
                view()
                    .gap_2()
                    .child(
                        input()
                            .multiline(true)
                            .value(body)
                            .on_change(move |t: &str| body.set(t.to_owned()))
                            .on_content_size_change(move |size: &Size<Points>| {
                                body_height.set(size.height.0.clamp(44.0, 200.0));
                            })
                            .h(move || px(body_height.get() + 16.0))
                            .p_3()
                            .rounded(px(10.0))
                            .bg(colors::fill())
                            .text_size(px(15.0))
                            .line_height(px(20.0))
                            .text_color(colors::label())
                            .caret_color(|| color(colors::tint()))
                            .placeholder("Your answer")
                            .label("Your answer"),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_2()
                            .child(button("Cancel", Fill::Plain, idle, move || {
                                replying.set(false)
                            }))
                            .child(button(
                                "Send",
                                Fill::Prominent,
                                move || idle() && body.with(|b| !b.trim().is_empty()),
                                reply.clone(),
                            )),
                    )
                    .into_element()
            })
        }))
        .child(failure(request))
}
