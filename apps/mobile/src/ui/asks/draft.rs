//! A draft to review: read it, edit its text, send it or discard it.

use else_wire::outgoing::{DraftAttachment, DraftContent, DraftEdit, OutgoingDraft};
use nsh_base::{alert_dialog, input};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::super::parts::colors;
use super::super::session;
use super::{Fill, button, eyebrow, failure, frame};
use else_client::request::Request;

use crate::api::Message;

/// What a draft card asks of the server.
#[derive(Debug, Clone, PartialEq)]
enum DraftOp {
    Approve(DraftEdit),
    /// The turn that drafted it has ended: a reply carrying it asks the bot
    /// to send it.
    SendAsIs(Message, Uuid),
    Discard,
}
/// Whether the draft card shows the draft or the owner's edits to it.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Reading,
    Editing,
}

pub(super) fn card(bot: Uuid, draft: OutgoingDraft) -> impl IntoElement {
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
                    reaction: None,
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
