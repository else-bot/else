//! A bot's blocker or decision, answered by a reply or marked handled.

use else_wire::notices::{OwnerNotice, OwnerNoticeKind};
use nsh_base::input;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::super::parts::colors;
use super::super::session;
use super::{Fill, button, eyebrow, failure, frame};
use else_client::request::Request;

use crate::api::Message;

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
pub(super) fn card(notice: OwnerNotice) -> impl IntoElement {
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
