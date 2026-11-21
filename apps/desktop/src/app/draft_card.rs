use crate::app::transcript::CardKey;
use crate::controls::{Control as _, Scale, Tier};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Icon, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::images::{MessageImages, is_image};
use super::message_row::{CardPlacement, short_stamp};
use super::request::Request;
use super::{Connection, ElseApp, RowKey};
use crate::api::ClientError;
use crate::attachments::{MessagePart, byte_size};
use crate::model::{DraftAttachment, DraftEdit, DraftFile, DraftStatus, OutgoingDraft};

#[derive(Clone, Copy)]
enum Choice {
    Approve,
    ApproveAlways,
    Discard,
}

#[derive(Clone)]
enum Operation {
    Approve { edit: DraftEdit, always: bool },
    Discard,
}

pub(super) struct DraftImages {
    gallery: Entity<MessageImages>,
    _observe: Subscription,
}

fn image_parts(files: &[DraftFile]) -> Vec<MessagePart> {
    files
        .iter()
        .map(DraftFile::attachment)
        .filter(is_image)
        .map(|file| MessagePart::File { file })
        .collect()
}

impl DraftImages {
    fn sync(&self, files: &[DraftFile], cx: &mut Context<ElseApp>) {
        self.gallery
            .update(cx, |images, cx| images.sync(&image_parts(files), cx));
    }
}

pub(super) struct DraftForm {
    subject: Option<Entity<InputState>>,
    body: Entity<TextareaState>,
    baseline: DraftEdit,
    files: Vec<DraftFile>,
    request: Request<Operation>,
    _changes: Vec<Subscription>,
}

impl DraftForm {
    fn new(draft: &OutgoingDraft, window: &mut Window, cx: &mut Context<ElseApp>) -> Self {
        let subject = draft.content.subject().map(|text| {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Subject"));
            input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
            input
        });
        let body = cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 30));
        body.update(cx, |input, cx| {
            input.set_value(draft.content.body().to_owned(), window, cx)
        });
        let id = draft.id;
        let subject_change = subject.as_ref().map(|subject| {
            cx.subscribe_in(subject, window, move |app, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    app.remeasure_row(RowKey::Card(CardKey::Draft(id)), cx);
                }
            })
        });
        let body_change =
            cx.subscribe_in(&body, window, move |app, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    app.remeasure_row(RowKey::Card(CardKey::Draft(id)), cx);
                }
            });
        Self {
            subject,
            body,
            baseline: draft.edit(),
            files: draft.content.attachments().to_vec(),
            request: Request::Idle,
            _changes: subject_change.into_iter().chain([body_change]).collect(),
        }
    }

    /// Carries `old`'s request into this rebuilt form, so its result still
    /// settles here.
    fn keeping(mut self, old: &mut Self) -> Self {
        self.request = std::mem::take(&mut old.request);
        self
    }

    /// Whether the draft's controls lock: an action is in flight, accepted,
    /// or of unknown outcome and only resendable unchanged.
    fn locked(&self) -> bool {
        self.request.busy() || self.request.unconfirmed()
    }

    fn edit(&self, cx: &App) -> DraftEdit {
        DraftEdit {
            version: self.baseline.version,
            subject: self
                .subject
                .as_ref()
                .map(|input| input.read(cx).value().to_string()),
            body: self.body.read(cx).value().to_string(),
            attachments: self
                .baseline
                .attachments
                .as_ref()
                .map(|_| self.files.iter().map(|file| file.id).collect()),
        }
    }
}

impl ElseApp {
    pub(super) fn sync_draft_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let drafts: Vec<_> = view
            .conversation
            .iter()
            .flat_map(|c| c.turns_in_order())
            .flat_map(|t| &t.outgoing_drafts)
            .cloned()
            .collect();
        view.cards
            .drafts
            .retain(|id, _| drafts.iter().any(|d| d.id == *id));
        for draft in &drafts {
            let card = view.cards.drafts.entry(draft.id).or_default();
            if !draft.is_open() {
                card.form = None;
            } else {
                match card.form.as_mut() {
                    Some(form) if form.baseline.version != draft.version => {
                        let local = form.edit(cx);
                        if local == form.baseline
                            || (local.subject.as_deref() == draft.content.subject()
                                && local.body == draft.content.body()
                                && local.attachments.unwrap_or_default()
                                    == draft.content.attachment_ids().unwrap_or_default())
                        {
                            *form = DraftForm::new(draft, window, cx).keeping(form);
                        } else if !form.request.unconfirmed() {
                            form.request.reject("This draft changed elsewhere. Your edits are still here. Load the latest draft before approving.");
                        }
                    }
                    Some(_) => {}
                    None => card.form = Some(DraftForm::new(draft, window, cx)),
                }
            }
            let files = card
                .form
                .as_ref()
                .map_or(draft.content.attachments(), |form| form.files.as_slice());
            let parts = image_parts(files);
            if let Some(images) = &card.images {
                images.sync(files, cx);
            } else if !parts.is_empty() {
                let (gallery, _observe) = Self::message_images(
                    self.client.clone(),
                    RowKey::Card(CardKey::Draft(draft.id)),
                    view.bot_id,
                    &parts,
                    window,
                    cx,
                );
                card.images = Some(DraftImages { gallery, _observe });
            }
        }
    }

    pub(super) fn outgoing_attachment(&self, cx: &App) -> Option<DraftAttachment> {
        let view = self.view.as_ref()?;
        let draft = view.conversation.as_ref()?.open_draft()?;
        if view
            .cards
            .draft_form(draft.id)
            .is_some_and(DraftForm::locked)
        {
            return None;
        }
        let edit = view
            .cards
            .draft_form(draft.id)
            .map_or_else(|| draft.edit(), |form| form.edit(cx));
        Some(DraftAttachment { id: draft.id, edit })
    }

    fn draft_in_flight(&self, id: Uuid) -> bool {
        self.selected
            .and_then(|bot| self.pending.get(&bot))
            .is_some_and(|list| {
                list.iter().any(|pending| {
                    !pending.is_rejected()
                        && pending.open_draft.as_ref().is_some_and(|a| a.id == id)
                })
            })
    }

    fn act_on_draft(
        &mut self,
        id: Uuid,
        choice: Choice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        let sent = match (view.cards.draft_form_mut(id), choice) {
            (Some(form), _) if form.request.unconfirmed() => form.request.retry(),
            (Some(form), Choice::Approve | Choice::ApproveAlways)
                if !form.body.read(cx).value().trim().is_empty() =>
            {
                let operation = Operation::Approve {
                    edit: form.edit(cx),
                    always: matches!(choice, Choice::ApproveAlways),
                };
                form.request
                    .begin(operation.clone())
                    .map(|attempt| (attempt, operation))
            }
            (Some(form), Choice::Discard) => form
                .request
                .begin(Operation::Discard)
                .map(|attempt| (attempt, Operation::Discard)),
            (None, Choice::Discard) => Some((Uuid::new_v4(), Operation::Discard)),
            _ => None,
        };
        let Some((attempt, operation)) = sent else {
            return;
        };
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = match operation {
                Operation::Approve { edit, always } => {
                    client.approve_draft(bot, id, edit, always).await
                }
                Operation::Discard => client.discard_draft(bot, id).await,
            };
            let _ = this.update_in(cx, |app, window, cx| {
                app.draft_acted(bot, id, Some(attempt), result, window, cx)
            });
        })
        .detach();
        self.remeasure_row(RowKey::Card(CardKey::Draft(id)), cx);
    }

    /// Applies the result of a draft action. `attempt` is the form's
    /// request it settles; `None` for an action the form does not track.
    fn draft_acted(
        &mut self,
        bot: Uuid,
        id: Uuid,
        attempt: Option<Uuid>,
        result: Result<OutgoingDraft, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = self
            .view_of(bot)
            .and_then(|view| view.cards.draft_form_mut(id));
        let tracked = match (form, attempt) {
            (Some(form), Some(attempt)) => Some(form.request.settle(attempt, &result)),
            (Some(form), None) => {
                if let Err(err) = &result
                    && !form.locked()
                {
                    form.request.reject(err.to_string());
                }
                Some(true)
            }
            (None, _) => None,
        };
        if result
            .as_ref()
            .is_err_and(|err| self.unauthorized(err, window, cx))
            || tracked == Some(false)
        {
            return;
        }
        match result {
            Ok(draft) => {
                if let Some(conversation) = self
                    .view_of(bot)
                    .and_then(|view| view.conversation.as_mut())
                {
                    conversation.update_draft(draft);
                }
                self.sync_rows(window, cx);
            }
            Err(err) => {
                if tracked.is_none() {
                    self.notice = Some(err.to_string());
                }
                self.remeasure_row(RowKey::Card(CardKey::Draft(id)), cx);
            }
        }
    }

    fn stop_approving_always(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.view.as_ref().map(|v| v.bot_id) else {
            return;
        };
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.stop_approving_always(bot, id).await;
            let _ = this.update_in(cx, |app, window, cx| {
                if let Err(err) = result {
                    app.draft_acted(bot, id, None, Err(err), window, cx);
                }
            });
        })
        .detach();
    }

    fn reload_draft_form(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self
            .view
            .as_ref()
            .and_then(|v| v.conversation.as_ref())
            .and_then(|c| c.outgoing_draft(id))
            .cloned();
        if let (Some(draft), Some(view)) = (draft, self.view.as_mut()) {
            let card = view.cards.drafts.entry(id).or_default();
            let fresh = DraftForm::new(&draft, window, cx);
            card.form = Some(match card.form.as_mut() {
                Some(old) => fresh.keeping(old),
                None => fresh,
            });
            self.sync_draft_cards(window, cx);
        }
        self.remeasure_row(RowKey::Card(CardKey::Draft(id)), cx);
    }

    fn render_draft_files(
        &self,
        draft: &OutgoingDraft,
        files: &[DraftFile],
        editable: bool,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = draft.id;
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .children(files.iter().map(|file| {
                let file_id = file.id;
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().text_sm().whitespace_normal().child(file.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .whitespace_normal()
                                    .child(format!(
                                        "{} · {}",
                                        file.media_type,
                                        byte_size(file.size)
                                    )),
                            ),
                    )
                    .when(editable, |row| {
                        row.child(
                            Button::new(SharedString::from(format!("draft-remove-{id}-{file_id}")))
                                .control(Tier::Ghost, Scale::Compact, busy, cx)
                                .label("Remove")
                                .accessibility_label(format!("Remove attachment {}", file.name))
                                .on_click(cx.listener(move |app, _, _, cx| {
                                    let Some(view) = app.view.as_mut() else {
                                        return;
                                    };
                                    let Some(card) = view.cards.drafts.get_mut(&id) else {
                                        return;
                                    };
                                    let Some(form) = card.form.as_mut() else {
                                        return;
                                    };
                                    form.files.retain(|file| file.id != file_id);
                                    if let Some(images) = &card.images {
                                        images.sync(&form.files, cx);
                                    }
                                    app.remeasure_row(RowKey::Card(CardKey::Draft(id)), cx);
                                })),
                        )
                    })
            }))
            .when_some(
                self.view
                    .as_ref()
                    .and_then(|view| view.cards.drafts.get(&id)?.images.as_ref()),
                |column, images| column.child(images.gallery.clone()),
            )
            .into_any_element()
    }

    pub(super) fn render_draft_card(
        &self,
        id: Uuid,
        placement: CardPlacement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let Some(draft) = view
            .conversation
            .as_ref()
            .and_then(|c| c.outgoing_draft(id))
        else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let row = v_flex().w_full();
        let muted = |text: String| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let content_icon = if draft.content.is_email() {
            IconName::Mail
        } else {
            IconName::MessageSquare
        };
        let metadata = || {
            draft
                .content
                .metadata()
                .into_iter()
                // A fixed label column, so every value starts on one edge.
                .map(|(label, value)| {
                    h_flex()
                        .items_start()
                        .text_size(px(13.))
                        .line_height(px(20.))
                        .child(
                            div()
                                .w(px(80.))
                                .flex_none()
                                .whitespace_nowrap()
                                .text_color(theme.muted_foreground)
                                .child(label),
                        )
                        .child(div().flex_1().min_w_0().whitespace_normal().child(value))
                })
                .collect::<Vec<_>>()
        };
        // A settled draft is a receipt: a quiet fill, like a settled question.
        let mut card = v_flex()
            .w_full()
            .rounded(theme.radius_lg)
            .bg(theme.secondary)
            .overflow_hidden();
        let expanded = view.cards.drafts.get(&id).is_some_and(|card| card.expanded);
        if !draft.is_open() {
            let color = match draft.status {
                DraftStatus::Sent => theme.success,
                DraftStatus::Failed => theme.danger,
                DraftStatus::Uncertain => theme.warning,
                _ => theme.muted_foreground,
            };
            let header = h_flex()
                .id(SharedString::from(format!("draft-receipt-{id}")))
                .p_3()
                .gap_2()
                .cursor_pointer()
                .child(
                    Icon::new(if draft.status == DraftStatus::Sent {
                        IconName::Check
                    } else {
                        content_icon
                    })
                    .size_4()
                    .text_color(color),
                )
                .map(|header| match placement {
                    CardPlacement::InTask => {
                        let (title, outcome) = draft.task_receipt();
                        header.child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_medium()
                                        .whitespace_normal()
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .whitespace_normal()
                                        .text_color(theme.muted_foreground)
                                        .child(outcome),
                                ),
                        )
                    }
                    CardPlacement::Row => header
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .child(draft.content.label().to_owned()),
                        )
                        .child(muted(match draft.settled_at {
                            Some(at) => format!("{} · {}", draft.label(), short_stamp(at)),
                            None => draft.label(),
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(draft.content.audience()),
                        ),
                })
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size_3()
                    .text_color(theme.muted_foreground),
                )
                .on_click(cx.listener(move |app, _, _, cx| {
                    if let Some(view) = app.view.as_mut() {
                        let card = view.cards.drafts.entry(id).or_default();
                        card.expanded = !card.expanded;
                    }
                    app.remeasure_row(RowKey::Card(CardKey::Draft(id)), cx);
                }));
            card = card.child(header);
            if !draft.content.attachments().is_empty() {
                card = card.child(div().px_4().pb_3().child(self.render_draft_files(
                    draft,
                    draft.content.attachments(),
                    false,
                    false,
                    cx,
                )));
            }
            if expanded {
                card = card.child(
                    v_flex()
                        .p_4()
                        .gap_3()
                        .border_t_1()
                        .border_color(theme.border)
                        .children(draft.content.subject().map(|subject| {
                            div().text_sm().font_semibold().child(subject.to_owned())
                        }))
                        .children(metadata())
                        .child(
                            div()
                                .text_sm()
                                .whitespace_normal()
                                .child(draft.content.body().to_owned()),
                        ),
                );
            }
            if view
                .conversation
                .as_ref()
                .is_some_and(|c| c.approves_always(draft))
            {
                let connected = self.connection == Connection::Connected;
                card = card.child(
                    h_flex()
                        .px_4()
                        .pb_3()
                        .gap_2()
                        .child(
                            muted(format!(
                                "You approve {} always.",
                                draft.content.kind_label()
                            ))
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal(),
                        )
                        .child(
                            Button::new(SharedString::from(format!("draft-stop-always-{id}")))
                                .flex_none()
                                .control(Tier::Ghost, Scale::Compact, !connected, cx)
                                .label("Stop approving always")
                                .on_click(cx.listener(move |app, _, window, cx| {
                                    app.stop_approving_always(id, window, cx)
                                })),
                        ),
                );
            }
            if draft.status == DraftStatus::Discarded
                && let Some(detail) = &draft.result_detail
            {
                card = card.child(div().px_4().pb_3().child(muted(detail.clone())));
            }
            if matches!(draft.status, DraftStatus::Failed | DraftStatus::Uncertain) {
                card = card.child(
                    v_flex()
                        .px_4()
                        .pb_3()
                        .gap_1()
                        .child(muted(draft.result_detail.clone().unwrap_or_default()))
                        .child(muted(
                            if draft.status == DraftStatus::Uncertain {
                                "Check whether it arrived before asking the bot to retry."
                            } else {
                                "Reply in chat to revise or try again."
                            }
                            .into(),
                        )),
                );
            }
            return row.child(card).into_any_element();
        }
        let Some(form) = view.cards.draft_form(id) else {
            return row.into_any_element();
        };
        let connected = self.connection == Connection::Connected;
        let busy = form.locked() || self.draft_in_flight(id);
        let conflict = form.baseline.version != draft.version;
        let approve_enabled = connected
            && !busy
            && !conflict
            && draft.can_approve
            && !form.body.read(cx).value().trim().is_empty();
        // The bot's name row sits above the card. An email's editable subject
        // is its one title, with what it is as quiet meta beside it; anything
        // else is titled by what it is.
        let kind = format!("{} draft", draft.content.label());
        let revision = (draft.revision > 1).then(|| format!("v{}", draft.revision));
        let card = match form.subject.as_ref() {
            Some(subject) => super::action_card::titled(
                content_icon,
                // The input is taller than a title line; centre it on one.
                // A hover shade, hung past the title's edge, shows it edits.
                div()
                    .h(px(22.))
                    .mx(px(-6.))
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .rounded(theme.radius)
                    .hover(|style| style.bg(theme.foreground.opacity(0.06)))
                    .child(
                        Input::new(subject)
                            .appearance(false)
                            .px_0()
                            .text_size(px(15.))
                            .disabled(busy)
                            .aria_label("Email subject")
                            .font_semibold(),
                    ),
                "",
            )
            .meta(
                revision
                    .clone()
                    .map_or(kind.clone(), |revision| format!("{kind} · {revision}")),
            ),
            None => super::action_card::card(content_icon, kind, "")
                .when_some(revision, |card, revision| card.meta(revision)),
        };
        let mut card = card
            .when(!draft.can_approve, |card| card.tag("Reply in chat"))
            .child(
                v_flex().gap_2().children(metadata()).child(
                    // A deeper fill and a visible edge mark the body as
                    // editable, apart from the card's read-only lines.
                    div()
                        .mt_1()
                        .rounded(theme.radius)
                        .bg(if theme.mode.is_dark() {
                            theme.background.opacity(0.6)
                        } else {
                            gpui_kit::white()
                        })
                        .border_1()
                        .border_color(theme.foreground.opacity(0.1))
                        .hover(|style| style.border_color(theme.foreground.opacity(0.2)))
                        .child(
                            Textarea::new(&form.body)
                                .appearance(false)
                                .line_height(relative(1.5))
                                .disabled(busy)
                                .aria_label(draft.content.body_label()),
                        ),
                ),
            );
        if !form.files.is_empty() {
            card = card.child(self.render_draft_files(draft, &form.files, true, busy, cx));
        }
        let footer = super::action_card::footer()
            .when(draft.kind.is_some() && draft.can_approve, |row| {
                row.note(format!(
                    "Approve always also approves later {} without asking.{}",
                    draft.content.kind_label(),
                    if draft.content.sends_as_owner() {
                        " They go out from your own account, to this chat only."
                    } else {
                        ""
                    }
                ))
            })
            .child(
                Button::new(SharedString::from(format!("draft-discard-{id}")))
                    .control(Tier::Secondary, Scale::Compact, !connected || busy, cx)
                    .label("Discard")
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.act_on_draft(id, Choice::Discard, window, cx)
                    })),
            )
            .when(draft.kind.is_some(), |row| {
                row.child(
                    Button::new(SharedString::from(format!("draft-approve-always-{id}")))
                        .control(Tier::Secondary, Scale::Compact, !approve_enabled, cx)
                        .label("Approve always")
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.act_on_draft(id, Choice::ApproveAlways, window, cx)
                        })),
                )
            })
            .child(
                Button::new(SharedString::from(format!("draft-approve-{id}")))
                    .control(Tier::Primary, Scale::Compact, !approve_enabled, cx)
                    .label(if busy {
                        "Saving…"
                    } else {
                        draft.content.approve_label()
                    })
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.act_on_draft(id, Choice::Approve, window, cx)
                    })),
            );
        if let Some(error) = form.request.error() {
            card = card.child(super::action_card::failure(&theme, error.to_owned()));
        }
        let retry = form.request.unconfirmed();
        if retry || conflict {
            card = card.child(
                h_flex().justify_end().child(
                    Button::new(SharedString::from(format!("draft-recover-{id}")))
                        .control(Tier::Ghost, Scale::Compact, !connected, cx)
                        .label(if retry {
                            "Retry same action"
                        } else {
                            "Load latest draft"
                        })
                        .on_click(cx.listener(move |app, _, window, cx| {
                            if retry {
                                app.act_on_draft(id, Choice::Approve, window, cx);
                            } else {
                                app.reload_draft_form(id, window, cx);
                            }
                        })),
                ),
            );
        }
        row.child(card.footer(footer)).into_any_element()
    }
}
