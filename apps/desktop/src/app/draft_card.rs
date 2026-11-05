use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::{Connection, ElseApp, RowKey};
use crate::api::ClientError;
use crate::model::{DraftAttachment, DraftEdit, DraftStatus, OutgoingDraft};

#[derive(Clone)]
enum Operation {
    Approve(DraftEdit),
    Discard,
}

pub(super) struct DraftForm {
    subject: Option<Entity<InputState>>,
    body: Entity<TextareaState>,
    baseline: DraftEdit,
    operation: Option<Operation>,
    retryable: bool,
    error: Option<String>,
    _changes: Vec<Subscription>,
}

impl DraftForm {
    fn new(draft: &OutgoingDraft, window: &mut Window, cx: &mut Context<ElseApp>) -> Self {
        let subject = draft.content.subject().map(|text| {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Subject"));
            input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
            input
        });
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(if draft.content.is_email() { 6 } else { 3 }, 30)
        });
        body.update(cx, |input, cx| {
            input.set_value(draft.content.body().to_owned(), window, cx)
        });
        let id = draft.id;
        let subject_change = subject.as_ref().map(|subject| {
            cx.subscribe_in(subject, window, move |app, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    app.remeasure_row(RowKey::Draft(id), cx);
                }
            })
        });
        let body_change =
            cx.subscribe_in(&body, window, move |app, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    app.remeasure_row(RowKey::Draft(id), cx);
                }
            });
        Self {
            subject,
            body,
            baseline: draft.edit(),
            operation: None,
            retryable: false,
            error: None,
            _changes: subject_change.into_iter().chain([body_change]).collect(),
        }
    }

    fn edit(&self, cx: &App) -> DraftEdit {
        DraftEdit {
            version: self.baseline.version,
            subject: self
                .subject
                .as_ref()
                .map(|input| input.read(cx).value().to_string()),
            body: self.body.read(cx).value().to_string(),
        }
    }
}

impl ElseApp {
    pub(super) fn sync_draft_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let drafts: Vec<_> = view
            .conversation
            .iter()
            .flat_map(|c| c.turns_in_order())
            .flat_map(|t| &t.outgoing_drafts)
            .filter(|d| d.status == DraftStatus::Pending)
            .cloned()
            .collect();
        view.draft_forms
            .retain(|id, _| drafts.iter().any(|d| d.id == *id));
        for draft in drafts {
            match view.draft_forms.get_mut(&draft.id) {
                Some(form) if form.baseline.version != draft.version => {
                    let local = form.edit(cx);
                    if local == form.baseline
                        || (local.subject.as_deref() == draft.content.subject()
                            && local.body == draft.content.body())
                    {
                        *form = DraftForm::new(&draft, window, cx);
                    } else {
                        form.error = Some("This draft changed elsewhere. Your edits are still here. Load the latest draft before approving.".into());
                    }
                }
                Some(_) => {}
                None => {
                    view.draft_forms
                        .insert(draft.id, DraftForm::new(&draft, window, cx));
                }
            }
        }
    }

    pub(super) fn outgoing_attachment(&self, cx: &App) -> Option<DraftAttachment> {
        let view = self.view.as_ref()?;
        let draft = view.conversation.as_ref()?.open_draft()?;
        if view
            .draft_forms
            .get(&draft.id)
            .is_some_and(|form| form.operation.is_some())
        {
            return None;
        }
        let edit = view
            .draft_forms
            .get(&draft.id)
            .map_or_else(|| draft.edit(), |form| form.edit(cx));
        Some(DraftAttachment { id: draft.id, edit })
    }

    fn draft_in_flight(&self, id: Uuid) -> bool {
        self.selected
            .and_then(|bot| self.pending.get(&bot))
            .is_some_and(|list| {
                list.iter().any(|pending| {
                    pending.rejected.is_none()
                        && pending.open_draft.as_ref().is_some_and(|a| a.id == id)
                })
            })
    }

    fn act_on_draft(
        &mut self,
        id: Uuid,
        approve: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view.as_ref() else {
            return;
        };
        let operation = match view.draft_forms.get(&id) {
            Some(form) if form.retryable => form.operation.clone(),
            Some(form) if form.operation.is_none() => Some(if approve {
                Operation::Approve(form.edit(cx))
            } else {
                Operation::Discard
            }),
            None if !approve => Some(Operation::Discard),
            _ => None,
        };
        let Some(operation) = operation else { return };
        let bot = view.bot_id;
        if let Some(form) = self.view.as_mut().and_then(|v| v.draft_forms.get_mut(&id)) {
            form.operation = Some(operation.clone());
            form.retryable = false;
            form.error = None;
        }
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = match operation {
                Operation::Approve(edit) => client.approve_draft(bot, id, edit).await,
                Operation::Discard => client.discard_draft(bot, id).await,
            };
            let _ = this.update_in(cx, |app, window, cx| {
                app.draft_acted(bot, id, result, window, cx)
            });
        })
        .detach();
        self.remeasure_row(RowKey::Draft(id), cx);
    }

    fn draft_acted(
        &mut self,
        bot: Uuid,
        id: Uuid,
        result: Result<OutgoingDraft, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(&result, Err(ClientError::Unauthorized)) {
            self.connection = Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
        }
        let Some(view) = self.view.as_mut().filter(|v| v.bot_id == bot) else {
            return;
        };
        match result {
            Ok(draft) => {
                if let Some(conversation) = view.conversation.as_mut() {
                    conversation.update_draft(draft);
                }
                self.sync_rows(window, cx);
            }
            Err(err) => {
                if let Some(form) = view.draft_forms.get_mut(&id) {
                    form.retryable = err.outcome_unknown();
                    if !form.retryable {
                        form.operation = None;
                    }
                    form.error = Some(err.to_string());
                } else {
                    self.notice = Some(err.to_string());
                }
                self.remeasure_row(RowKey::Draft(id), cx);
            }
        }
    }

    fn reload_draft_form(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self
            .view
            .as_ref()
            .and_then(|v| v.conversation.as_ref())
            .and_then(|c| c.outgoing_draft(id))
            .cloned();
        if let (Some(draft), Some(view)) = (draft, self.view.as_mut()) {
            view.draft_forms
                .insert(id, DraftForm::new(&draft, window, cx));
        }
        self.remeasure_row(RowKey::Draft(id), cx);
    }

    pub(super) fn render_draft_card(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
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
        let row = super::view::reading_column().py_1p5();
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
                .map(|(label, value)| {
                    h_flex()
                        .px_1()
                        .gap_2()
                        .child(muted(label.into()))
                        .child(div().min_w_0().text_xs().whitespace_normal().child(value))
                })
                .collect::<Vec<_>>()
        };
        let mut card = v_flex()
            .w_full()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .overflow_hidden();
        let expanded = view.expanded_drafts.contains(&id);
        if draft.status != DraftStatus::Pending {
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
                        content_icon.clone()
                    })
                    .size_4()
                    .text_color(color),
                )
                .child(
                    div()
                        .text_sm()
                        .font_medium()
                        .child(draft.content.label().to_owned()),
                )
                .child(muted(draft.status.label(&draft.content).into()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(draft.content.audience()),
                )
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
                        if !view.expanded_drafts.remove(&id) {
                            view.expanded_drafts.insert(id);
                        }
                    }
                    app.remeasure_row(RowKey::Draft(id), cx);
                }));
            card = card.child(header);
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
        let Some(form) = view.draft_forms.get(&id) else {
            return row.into_any_element();
        };
        let connected = self.connection == Connection::Connected;
        let busy = form.operation.is_some() || self.draft_in_flight(id);
        let conflict = form.baseline.version != draft.version;
        let approve_enabled = connected && !busy && !conflict && draft.can_approve;
        card = card
            .child(
                h_flex()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .gap_2()
                    .child(Icon::new(content_icon).size_4())
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .child(draft.content.label().to_owned()),
                    )
                    .child(div().flex_1())
                    .child(muted(if draft.revision > 1 {
                        format!("Needs approval · v{}", draft.revision)
                    } else {
                        "Needs approval".into()
                    })),
            )
            .child(
                v_flex()
                    .px_3()
                    .gap_1()
                    .children(form.subject.as_ref().map(|subject| {
                        Input::new(subject)
                            .appearance(false)
                            .disabled(busy)
                            .aria_label("Email subject")
                            .font_semibold()
                    }))
                    .children(metadata())
                    .when(draft.content.is_email(), |el| {
                        el.child(div().mt_2().border_t_1().border_color(theme.border))
                    })
                    .child(
                        Textarea::new(&form.body)
                            .appearance(false)
                            .disabled(busy)
                            .aria_label(draft.content.body_label()),
                    ),
            )
            .child(
                h_flex()
                    .px_4()
                    .pt_2()
                    .pb_3()
                    .gap_2()
                    .flex_wrap()
                    .child(muted(
                        if draft.can_approve {
                            "Waiting for your approval"
                        } else {
                            "Reply in chat to continue this draft"
                        }
                        .into(),
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new(SharedString::from(format!("draft-discard-{id}")))
                            .focus_ring(false)
                            .label("Discard")
                            .small()
                            .ghost()
                            .disabled(!connected || busy)
                            .when(connected && !busy, |b| b.cursor_pointer())
                            .on_click(cx.listener(move |app, _, window, cx| {
                                app.act_on_draft(id, false, window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("draft-approve-{id}")))
                            .focus_ring(false)
                            .label(if busy {
                                "Saving…"
                            } else {
                                draft.content.approve_label()
                            })
                            .small()
                            .primary()
                            .disabled(!approve_enabled)
                            .when(approve_enabled, |b| b.cursor_pointer())
                            .on_click(cx.listener(move |app, _, window, cx| {
                                app.act_on_draft(id, true, window, cx)
                            })),
                    ),
            );
        if let Some(error) = &form.error {
            card = card.child(
                div()
                    .px_4()
                    .pb_2()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if form.retryable || conflict {
            let retry = form.retryable;
            card = card.child(
                h_flex().px_4().pb_3().justify_end().child(
                    Button::new(SharedString::from(format!("draft-recover-{id}")))
                        .focus_ring(false)
                        .label(if retry {
                            "Retry same action"
                        } else {
                            "Load latest draft"
                        })
                        .small()
                        .ghost()
                        .disabled(!connected)
                        .when(connected, |b| b.cursor_pointer())
                        .on_click(cx.listener(move |app, _, window, cx| {
                            if retry {
                                app.act_on_draft(id, true, window, cx);
                            } else {
                                app.reload_draft_form(id, window, cx);
                            }
                        })),
                ),
            );
        }
        row.child(card).into_any_element()
    }
}
