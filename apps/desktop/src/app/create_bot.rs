use super::ElseApp;
use super::request::{Request, Settle};
use crate::api::CreateBotRequest;
use crate::avatar::{AvatarLook, AvatarPicker, bot_avatar};
use crate::controls::{Control as _, Scale, Tier, field_style, text_field};
use crate::profile_picker::ProfilePicker;
use else_wire::bots::Avatar;
use else_wire::bots::AvatarColor;
use else_wire::bots::AvatarShape;
use else_wire::bots::Category;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{ActiveTheme as _, IndexPath, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use uuid::Uuid;

#[derive(Clone, PartialEq, Eq)]
struct CategoryChoice {
    id: Option<Uuid>,
    name: SharedString,
}

impl SearchableListItem for CategoryChoice {
    type Value = Option<Uuid>;

    fn title(&self) -> SharedString {
        self.name.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.id
    }
}

fn category_choices(categories: &[Category]) -> Vec<CategoryChoice> {
    std::iter::once(CategoryChoice {
        id: None,
        name: "Unassigned".into(),
    })
    .chain(categories.iter().map(|category| CategoryChoice {
        id: Some(category.id),
        name: category.name.clone().into(),
    }))
    .collect()
}

struct CreateForm {
    /// `Unconfirmed` keeps the request to resend unchanged, under its
    /// request ID, so a retry cannot create a second bot.
    request: Request<CreateBotRequest>,
    name: Entity<InputState>,
    avatar: Avatar,
    profile: Option<Entity<ProfilePicker>>,
    category: Entity<SelectState<Vec<CategoryChoice>>>,
    category_choices: Vec<CategoryChoice>,
}

impl CreateForm {
    fn locked(&self) -> bool {
        self.request.busy() || self.request.unconfirmed()
    }
}

pub(super) struct CreateFormHolder {
    form: CreateForm,
    _enter: Subscription,
}

impl ElseApp {
    pub(super) fn open_create_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let resumed = self.pending_create();
        let name = cx.new(|cx| {
            let input = InputState::new(window, cx).placeholder("e.g. Research");
            match &resumed {
                Some(op) => input.default_value(op.name.clone()),
                None => input,
            }
        });
        let profile = self.caps.clone().map(|caps| {
            let initial = match &resumed {
                Some(op) => op.profile.as_ref(),
                None => caps.defaults.as_ref(),
            };
            let mut picker = self.profile_picker(&caps, initial, cx).tabs();
            let locked = resumed.is_some();
            cx.new(|cx| {
                picker.set_disabled(locked, cx);
                picker
            })
        });
        let choices = category_choices(&self.categories);
        let selected = resumed
            .as_ref()
            .and_then(|op| {
                choices
                    .iter()
                    .position(|choice| choice.id == op.category_id)
            })
            .unwrap_or(0);
        let category = cx.new(|cx| {
            SelectState::new(choices.clone(), Some(IndexPath::new(selected)), window, cx)
        });
        let avatar = resumed
            .as_ref()
            .and_then(|op| op.avatar)
            .unwrap_or_else(|| Avatar {
                shape: AvatarShape::ALL[rand::random_range(0..AvatarShape::ALL.len())],
                color: AvatarColor::ALL[rand::random_range(0..AvatarColor::ALL.len())],
            });
        let request = match resumed {
            Some(op) => Request::Unconfirmed {
                op,
                error: "The last Create bot request may have reached the server.".into(),
            },
            None => Request::Idle,
        };
        let enter =
            cx.subscribe_in(
                &name,
                window,
                |app, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => app.submit_create(window, cx),
                    InputEvent::Change => window.refresh(),
                    _ => {}
                },
            );
        let holder = cx.new(|_| CreateFormHolder {
            form: CreateForm {
                request,
                name: name.clone(),
                avatar,
                profile,
                category,
                category_choices: choices,
            },
            _enter: enter,
        });
        self.create = Some(holder.clone());
        let app = cx.entity().downgrade();
        let focus_once = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_once.replace(false) {
                name.read(cx).focus_handle(cx).focus(window, cx);
            }
            let categories = app
                .upgrade()
                .map(|app| app.read(cx).categories.clone())
                .unwrap_or_default();
            let choices = category_choices(&categories);
            holder.update(cx, |holder, cx| {
                if holder.form.category_choices != choices {
                    holder.form.category.update(cx, |select, cx| {
                        select.set_items(choices.clone(), window, cx);
                        if select.selected_value().is_none() {
                            select.set_selected_index(Some(IndexPath::new(0)), window, cx);
                        }
                    });
                    holder.form.category_choices = choices;
                }
            });
            let form = &holder.read(cx).form;
            let theme = cx.theme();
            let locked = form.locked();
            let avatar_supported = app.upgrade().is_some_and(|app| {
                app.read(cx)
                    .caps
                    .as_ref()
                    .is_some_and(|caps| caps.bot_avatar_on_create)
            });
            let stacked = window.viewport_size().width < px(760.);
            let choose = holder.downgrade();
            // The avatar studio starts on the "Bot name" line, preview first,
            // and its last swatch row ends on the details' last control.
            let studio =
                v_flex()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .gap_5()
                    .when(stacked, |studio| studio.w_full().pb_2())
                    .when(!stacked, |studio| studio.w(px(256.)))
                    .child(
                        v_flex()
                            .items_center()
                            .gap_2()
                            .child(bot_avatar("create-avatar-preview", form.avatar, px(120.)))
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                format!(
                                    "{} · {}",
                                    form.avatar.color.label(),
                                    form.avatar.shape.label(),
                                ),
                            )),
                    )
                    .child(
                        AvatarPicker::new(form.avatar, move |avatar, window, cx| {
                            let _ = choose.update(cx, |holder, cx| {
                                holder.form.avatar = avatar;
                                cx.notify();
                            });
                            window.refresh();
                        })
                        .compact()
                        .disabled(locked || !avatar_supported),
                    )
                    .when(!avatar_supported, |studio| {
                        studio.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Update the server to choose an avatar."),
                        )
                    });
            let mut details = v_flex().flex_1().min_w_0().gap_5().child(
                v_flex().gap_2().child(field_label("Bot name", cx)).child(
                    text_field(Input::new(&form.name), cx)
                        .aria_label("Bot name")
                        .disabled(locked),
                ),
            );
            if !categories.is_empty() {
                details = details.child(
                    v_flex().gap_2().child(field_label("Category", cx)).child(
                        field_style(Select::new(&form.category), cx)
                            .focus_ring(false)
                            .cursor_pointer()
                            .accessibility_label("Category")
                            .disabled(locked),
                    ),
                );
            }
            if let Some(profile) = &form.profile {
                details = details.child(profile.clone());
            }
            if let Some(error) = form.request.error() {
                let error = if form.request.unconfirmed() {
                    format!("{error} Create will retry the original request unchanged.")
                } else {
                    error.to_owned()
                };
                details = details.child(div().text_sm().text_color(theme.danger).child(error));
            }
            let columns = if stacked {
                v_flex()
            } else {
                h_flex().items_stretch()
            };
            let body = v_flex()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("A new bot for whatever you have in mind."),
                )
                .child(columns.gap_8().child(studio).child(details));
            let app_create = app.clone();
            let busy = form.request.busy();
            let can_submit = !form.name.read(cx).value().trim().is_empty();
            let app_ok = app.clone();
            let app_close = app.clone();
            let mine = holder.clone();
            let app_cancel = app.clone();
            let mine_cancel = holder.clone();
            crate::appearance::modal_surface(dialog, cx)
                .title(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Create bot"),
                )
                .centered()
                .w(px(704.))
                .on_close(move |_, _, cx| {
                    let _ = app_close.update(cx, |app, _| {
                        if app.create.as_ref() == Some(&mine) {
                            app.create = None;
                        }
                    });
                })
                .on_ok(move |_, window, cx| {
                    let _ = app_ok.update(cx, |app, cx| app.submit_create(window, cx));
                    false
                })
                .child(body)
                .footer(
                    h_flex()
                        .w_full()
                        .pt_3()
                        .border_t_1()
                        .border_color(theme.border)
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("create-cancel")
                                .control(Tier::Secondary, Scale::Standard, false, cx)
                                .label("Cancel")
                                .on_click(move |_, window, cx| {
                                    let _ = app_cancel.update(cx, |app, _| {
                                        if app.create.as_ref() == Some(&mine_cancel) {
                                            app.create = None;
                                        }
                                    });
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("create-submit")
                                .control(Tier::Primary, Scale::Standard, busy || !can_submit, cx)
                                .label("Create bot")
                                .loading(busy)
                                .on_click(move |_, window, cx| {
                                    let _ = app_create
                                        .update(cx, |app, cx| app.submit_create(window, cx));
                                }),
                        ),
                )
        });
    }
    fn submit_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (client, Some(holder)) = (self.client.clone(), self.create.clone()) else {
            return;
        };
        let form = &holder.read(cx).form;
        if form.request.busy() || form.name.read(cx).value().trim().is_empty() {
            return;
        }
        let fresh = CreateBotRequest {
            request_id: Uuid::new_v4(),
            name: form.name.read(cx).value().to_string(),
            profile: form.profile.as_ref().and_then(|p| p.read(cx).choice()),
            category_id: form.category.read(cx).selected_value().copied().flatten(),
            avatar: self
                .caps
                .as_ref()
                .is_some_and(|caps| caps.bot_avatar_on_create)
                .then_some(form.avatar),
        };
        let payload = form.request.unconfirmed_op().cloned().unwrap_or(fresh);
        if let Err(err) = self.save_pending_create(payload.clone()) {
            let error = format!("Couldn't save the request before sending it. {err}");
            holder.update(cx, |h, cx| {
                h.form.request = match h.form.request.unconfirmed_op().cloned() {
                    Some(op) => Request::Unconfirmed { op, error },
                    None => Request::Failed(error),
                };
                cx.notify();
            });
            window.refresh();
            return;
        }
        let sent = holder.update(cx, |h, cx| {
            let sent = h.form.request.retry().or_else(|| {
                h.form
                    .request
                    .begin(payload.clone())
                    .map(|attempt| (attempt, payload))
            });
            if let Some(profile) = &h.form.profile {
                profile.update(cx, |picker, cx| picker.set_disabled(true, cx));
            }
            cx.notify();
            sent
        });
        let Some((attempt, payload)) = sent else {
            return;
        };
        let request_id = payload.request_id;
        self.create_attempt = Some(attempt);
        window.refresh();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.create_bot(payload).await;
            let _ = this.update_in(cx, |app, window, cx| {
                let settled = holder.update(cx, |h, _| h.form.request.settle(attempt, &result))
                    != Settle::Stale;
                let rejected = settled
                    && app.create_attempt == Some(attempt)
                    && !holder.read(cx).form.request.unconfirmed();
                if result.is_ok() || rejected {
                    app.clear_pending_create(request_id);
                }
                if let Err(err) = &result {
                    app.unauthorized(err, cx);
                }
                if !settled {
                    return;
                }
                match result {
                    Ok(bot) => {
                        let closing = app.create.as_ref().is_some_and(|current| {
                            let request = &current.read(cx).form.request;
                            current == &holder
                                || request
                                    .pending()
                                    .or(request.unconfirmed_op())
                                    .is_some_and(|op| op.request_id == request_id)
                        });
                        if closing {
                            app.create = None;
                            window.close_dialog(cx);
                        }
                        let listed = app.bots.iter().any(|b| b.id == bot.id);
                        if !listed {
                            app.bots = app.bots.iter().cloned().chain([bot.clone()]).collect();
                            app.refresh_now(window, cx);
                        }
                        if closing || !listed {
                            app.select(Some(bot.id), window, cx);
                            app.composer.read(cx).focus_handle(cx).focus(window, cx);
                        }
                    }
                    Err(_) => {
                        let mut forms = vec![holder.clone()];
                        let reopened = app.create.clone().filter(|current| {
                            rejected
                                && current != &holder
                                && current
                                    .read(cx)
                                    .form
                                    .request
                                    .unconfirmed_op()
                                    .is_some_and(|op| op.request_id == request_id)
                        });
                        if let Some(current) = reopened {
                            let request = holder.read(cx).form.request.clone();
                            current.update(cx, |h, _| h.form.request = request);
                            forms.push(current);
                        }
                        for form in forms {
                            form.update(cx, |h, cx| {
                                if let Some(profile) = &h.form.profile {
                                    let unconfirmed = h.form.request.unconfirmed();
                                    profile.update(cx, |picker, cx| {
                                        picker.set_disabled(unconfirmed, cx)
                                    });
                                }
                                cx.notify();
                            });
                        }
                        window.refresh();
                    }
                }
            });
        })
        .detach();
    }
}

impl ElseApp {
    /// The create sent to this server whose outcome is not yet known.
    fn pending_create(&self) -> Option<CreateBotRequest> {
        self.preferences
            .pending_creates
            .get(self.client.origin().as_str())
            .cloned()
    }

    /// Saved before sending, rather than debounced: a changed owner key
    /// replaces this session, and the next one reads the request back from
    /// disk. On failure nothing is kept, and nothing may be sent.
    fn save_pending_create(&mut self, request: CreateBotRequest) -> Result<(), String> {
        let origin = self.client.origin().as_str().to_owned();
        let previous = self
            .preferences
            .pending_creates
            .insert(origin.clone(), request);
        self.preferences.save().inspect_err(|_| match previous {
            Some(previous) => {
                self.preferences.pending_creates.insert(origin, previous);
            }
            None => {
                self.preferences.pending_creates.remove(&origin);
            }
        })
    }

    /// Drops the pending create once its outcome is known. A later
    /// create's record is left alone.
    fn clear_pending_create(&mut self, request_id: Uuid) {
        let origin = self.client.origin().as_str();
        if self
            .preferences
            .pending_creates
            .get(origin)
            .is_some_and(|pending| pending.request_id == request_id)
        {
            self.preferences.pending_creates.remove(origin);
            if let Err(err) = self.preferences.save() {
                eprintln!("{err}");
            }
        }
    }
}

/// A field's label, in the same voice as the model picker's "Provider",
/// "Reasoning" and "Speed".
fn field_label(text: &'static str, cx: &App) -> Div {
    div()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(text)
}
