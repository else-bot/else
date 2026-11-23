//! The chat's composer: the text field, the files picked for the next
//! message (uploaded as soon as they are picked), Send and Stop.

use else_wire::items::{Attachment, MAX_FILE_BYTES, MAX_FILES};
use nsh_base::{alert_dialog, input};
use nsh_core::element::AdaptiveColor;
use nsh_core::picker::{FilePickRequest, PickRequest, PickedFile, PickedStatus};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::{ARROW_UP, PLUS, STOP, colors};
use super::session::{self, Chat};

use else_client::request::Request;

use crate::api::{ClientError, Message};
use crate::conversation::{self};

/// The composer's message and the request ID it is sent under. A send
/// whose outcome is unknown keeps both: sending the same text and files
/// again repeats it under that ID, so the server cannot post it twice; any
/// change makes a new message.
#[derive(Debug, Clone, PartialEq)]
struct Outgoing {
    request_id: Uuid,
    message: Message,
}

/// A file the composer holds for the next message: uploaded as soon as it
/// is picked, so Send only names it.
#[derive(Clone)]
struct Pending {
    file: Attachment,
    upload: Upload,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.file == other.file && self.upload == other.upload
    }
}

#[derive(Clone)]
enum Upload {
    Uploading,
    Done,
    /// Not stored, or not known to be: its bytes stay so a tap retries
    /// it under the same ID, which the server takes once.
    Failed {
        message: String,
        data: std::sync::Arc<Vec<u8>>,
    },
}

impl PartialEq for Upload {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Uploading, Self::Uploading) | (Self::Done, Self::Done) => true,
            (Self::Failed { message: a, .. }, Self::Failed { message: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// Uploads one picked file and records how it went.
fn upload(
    bot: Uuid,
    pending: Signal<Vec<Pending>>,
    file: Attachment,
    data: std::sync::Arc<Vec<u8>>,
) {
    let id = file.id;
    pending.update(|p| {
        if let Some(item) = p.iter_mut().find(|item| item.file.id == id) {
            item.upload = Upload::Uploading;
        }
    });
    let client = session::server().client.clone();
    spawn(async move {
        let result = client.upload_attachment(bot, file, data.clone()).await;
        session::try_update(pending, |p| {
            let Some(item) = p.iter_mut().find(|item| item.file.id == id) else {
                return;
            };
            item.upload = match result {
                Ok(stored) => {
                    item.file = stored;
                    Upload::Done
                }
                Err(err) => Upload::Failed {
                    message: match err {
                        ClientError::Api { message, .. } => message,
                        err if err.outcome_unknown() => format!("{err} Tap to try again."),
                        err => err.to_string(),
                    },
                    data,
                },
            };
        });
    });
}

/// Asks where to pick from, picks, and uploads what was picked. Problems
/// with the picked files show in `notice`, apart from the send's state.
fn attach(bot: Uuid, pending: Signal<Vec<Pending>>, notice: Signal<Option<String>>) {
    let room = MAX_FILES.saturating_sub(pending.with_untracked(Vec::len)) as u32;
    if room == 0 {
        notice.set(Some(format!(
            "A message carries at most {MAX_FILES} files."
        )));
        return;
    }
    notice.set(None);
    spawn(async move {
        let choice = alert_dialog()
            .action_sheet()
            .action("photos", "Photo Library")
            .action("files", "Choose File")
            .cancel("cancel", "Cancel")
            .show()
            .await;
        let picked: Vec<PickedFile> = match choice.ok().and_then(|r| r.action) {
            Some(action) if action == "photos" => {
                let mut request = PickRequest::new(true, room);
                request.max_dimension = Some(DevicePixels(2048));
                pick_images(request).await
            }
            Some(action) if action == "files" => {
                pick_files(FilePickRequest::new(true, room).max_size(MAX_FILE_BYTES as u64)).await
            }
            _ => return,
        };
        for picked in picked {
            if picked.status == PickedStatus::TooLarge || picked.len > MAX_FILE_BYTES as u64 {
                notice.try_set(Some(format!(
                    "{} is larger than {} MB.",
                    picked.name,
                    MAX_FILE_BYTES / (1024 * 1024)
                )));
                continue;
            }
            let data = std::sync::Arc::new(picked.data.bytes().to_vec());
            let file = Attachment {
                id: Uuid::new_v4(),
                name: picked.name,
                media_type: picked.mime,
                size: data.len() as u64,
            };
            session::try_update(pending, |p| {
                p.push(Pending {
                    file: file.clone(),
                    upload: Upload::Uploading,
                })
            });
            upload(bot, pending, file, data);
        }
    });
}

/// The picked files above the text field, each with its state and a way
/// to take it out.
fn pending_strip(bot: Uuid, pending: Signal<Vec<Pending>>) -> impl IntoElement {
    dynamic(move || {
        let items = pending.get();
        (!items.is_empty()).then(|| {
            scroll().horizontal().scrollbar(false).pb_2().child(
                view()
                    .flex_row()
                    .gap_2()
                    .px_3()
                    .children(items.into_iter().map(move |item| {
                        let id = item.file.id;
                        let detail = match &item.upload {
                            Upload::Uploading => "Uploading…".to_owned(),
                            Upload::Done => super::files::size_label(item.file.size),
                            Upload::Failed { message, .. } => message.clone(),
                        };
                        let retry = match &item.upload {
                            Upload::Failed { data, .. } => Some(data.clone()),
                            Upload::Uploading | Upload::Done => None,
                        };
                        let failed = retry.is_some();
                        let file = item.file.clone();
                        view()
                            .flex_row()
                            .items_center()
                            .child(
                                super::files::chip(item.file.name.clone(), detail)
                                    .on_press(move || {
                                        if let Some(data) = retry.clone() {
                                            upload(bot, pending, file.clone(), data);
                                        }
                                    })
                                    .border_1()
                                    .border_color(move || {
                                        if failed {
                                            colors::destructive()
                                        } else {
                                            colors::separator()
                                        }
                                    }),
                            )
                            .child(
                                text("✕")
                                    .px_2()
                                    .py_2()
                                    .text_size(px(15.0))
                                    .text_color(colors::secondary())
                                    .role(AccessibilityRole::Button)
                                    .label(format!("Remove {}", item.file.name))
                                    .on_press(move || {
                                        pending.update(|p| p.retain(|item| item.file.id != id))
                                    }),
                            )
                            .into_element()
                    })),
            )
        })
    })
}

pub fn composer(bot: Uuid, chat: Signal<Chat>, busy: Memo<bool>) -> impl IntoElement {
    let draft = signal(String::new());
    let pending = signal(Vec::<Pending>::new());
    let attach_notice = signal(None::<String>);
    let outbox = signal(Request::<Outgoing>::Idle);
    let input_height = signal(22.0_f32);
    let sending = computed(move || outbox.with(Request::busy));
    let empty =
        computed(move || draft.with(|d| d.trim().is_empty()) && pending.with(Vec::is_empty));
    let uploaded =
        computed(move || pending.with(|p| p.iter().all(|item| item.upload == Upload::Done)));
    let can_send = computed(move || !empty.get() && uploaded.get() && !sending.get());
    let show_stop = computed(move || busy.get() && empty.get());

    let send = move || {
        if !can_send.get_untracked() {
            return;
        }
        let message = Message {
            text: draft.get_untracked().trim().to_owned(),
            files: pending.with_untracked(|p| p.iter().map(|item| item.file.clone()).collect()),
            // Only the draft card's Send carries a draft: a message about
            // something else must not close the draft or change its reply.
            draft: None,
        };
        let op = outbox.with_untracked(|o| match o.unconfirmed_op() {
            Some(unconfirmed) if unconfirmed.message == message => unconfirmed.clone(),
            _ => Outgoing {
                request_id: Uuid::new_v4(),
                message: message.clone(),
            },
        });
        let client = session::server().client.clone();
        // Held by the server, not by the Send button: clearing the text
        // swaps that button for Stop, which would cancel a send under way.
        session::submit(
            outbox,
            op,
            move |op| client.send(bot, op.message.request(op.request_id)),
            move |sent| {
                batch(|| {
                    if !outbox.try_set(Request::Idle) {
                        return;
                    }
                    // Only what was sent leaves the composer; text or files
                    // added meanwhile stay.
                    if draft.get_untracked().trim() == message.text {
                        draft.set(String::new());
                    }
                    pending.update(|p| p.retain(|item| !message.files.contains(&item.file)));
                    chat.update(|c| {
                        if let Some(conversation) = c.conversation_mut() {
                            conversation.insert_item(sent.item);
                            conversation.insert_turn(sent.turn);
                        }
                    });
                });
            },
        );
    };

    let stop = move || {
        let Some(request) = chat.with_untracked(|c| match c {
            Chat::Ready(conversation) | Chat::Recovering(conversation) => {
                conversation::active_request(conversation)
            }
            _ => None,
        }) else {
            return;
        };
        let client = session::server().client.clone();
        spawn(async move {
            let _ = client.cancel(bot, request).await;
        });
    };

    let notice = dynamic(move || {
        let message = match outbox.get() {
            Request::Unconfirmed { error, .. } => format!(
                "{error} It may have been sent. Sending it again unchanged cannot post it twice."
            ),
            Request::Failed(error) => error,
            _ => attach_notice.get()?,
        };
        Some(
            text(message)
                .px_4()
                .pb_2()
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::destructive()),
        )
    });

    let round_button = |icon: nsh_core::element::ImageAsset, size: f32, tint: AdaptiveColor| {
        view()
            .size(px(32.0))
            .rounded(px(16.0))
            .items_center()
            .justify_center()
            .active(|s| s.opacity(0.6))
            .child(image(icon).size(px(size)).tint_color(tint))
    };

    let action = when(show_stop, move || {
        round_button(STOP, 12.0, colors::chat())
            .bg(colors::label())
            .role(AccessibilityRole::Button)
            .label("Stop")
            .on_press(stop)
    })
    .otherwise(move || {
        round_button(ARROW_UP, 16.0, colors::on_tint())
            .bg(colors::tint())
            .opacity(move || if can_send.get() { 1.0_f32 } else { 0.35 })
            .role(AccessibilityRole::Button)
            .label("Send message")
            .on_press(send)
    });

    view()
        .bg(colors::chat())
        .border_t(px(0.5))
        .border_color(colors::separator())
        .pt_2()
        .pb(move || {
            px(8.0
                + if keyboard_height() > px(0.0) {
                    0.0
                } else {
                    safe_area().bottom.0
                })
        })
        .child(notice)
        .child(pending_strip(bot, pending))
        .child(
            view()
                .flex_row()
                .items_end()
                .gap_2()
                .px_3()
                .child(
                    view()
                        .size(px(40.0))
                        .rounded(px(20.0))
                        .items_center()
                        .justify_center()
                        .bg(colors::fill())
                        .active(|s| s.opacity(0.6))
                        .role(AccessibilityRole::Button)
                        .label("Attach")
                        .on_press(move || attach(bot, pending, attach_notice))
                        .child(image(PLUS).size(px(18.0)).tint_color(colors::secondary())),
                )
                .child(
                    view()
                        .flex_1()
                        .flex_row()
                        .items_end()
                        .pl(px(14.0))
                        .pr(px(4.0))
                        .py(px(4.0))
                        .gap_2()
                        .rounded(px(20.0))
                        .border_1()
                        .border_color(colors::separator())
                        .child(
                            input()
                                .flex_1()
                                .multiline(true)
                                .value(draft)
                                .on_change(move |t: &str| {
                                    draft.set(t.to_owned());
                                    if matches!(outbox.get_untracked(), Request::Failed(_)) {
                                        outbox.set(Request::Idle);
                                    }
                                })
                                .placeholder("Message")
                                .on_content_size_change(move |size: &Size<Points>| {
                                    input_height.set(size.height.0.clamp(22.0, 120.0));
                                })
                                .h(move || px(input_height.get() + 8.0))
                                .py(px(4.0))
                                .text_size(px(17.0))
                                .line_height(px(22.0))
                                .text_color(colors::label())
                                .placeholder_color(|| color(colors::secondary()))
                                .caret_color(|| color(colors::tint()))
                                .label("Message"),
                        )
                        .child(action),
                ),
        )
}
