use super::ElseApp;
use crate::api::{Category, CreateBotRequest};
use crate::avatar::{Avatar, AvatarColor, AvatarPicker, AvatarShape, bot_avatar};
use crate::controls::{Control as _, Scale, Tier, field_style, text_field};
use crate::profile_picker::ProfilePicker;
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
    request_id: Uuid,
    in_doubt: Option<CreateBotRequest>,
    name: Entity<InputState>,
    avatar: Avatar,
    profile: Option<Entity<ProfilePicker>>,
    category: Entity<SelectState<Vec<CategoryChoice>>>,
    category_choices: Vec<CategoryChoice>,
    error: Option<String>,
    busy: bool,
}

pub(super) struct CreateFormHolder {
    form: CreateForm,
}

impl ElseApp {
    pub(super) fn open_create_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Research"));
        let profile = self
            .caps
            .clone()
            .map(|caps| cx.new(|_| ProfilePicker::new(&caps, caps.defaults.as_ref()).tabs()));
        let choices = category_choices(&self.categories);
        let category =
            cx.new(|cx| SelectState::new(choices.clone(), Some(IndexPath::new(0)), window, cx));
        let holder = cx.new(|_| CreateFormHolder {
            form: CreateForm {
                request_id: Uuid::new_v4(),
                in_doubt: None,
                name: name.clone(),
                avatar: Avatar {
                    shape: AvatarShape::ALL[rand::random_range(0..AvatarShape::ALL.len())],
                    color: AvatarColor::ALL[rand::random_range(0..AvatarColor::ALL.len())],
                },
                profile,
                category,
                category_choices: choices,
                error: None,
                busy: false,
            },
        });
        self.create = Some(holder.clone());
        self._create_enter = Some(cx.subscribe_in(
            &name,
            window,
            |app, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => app.submit_create(window, cx),
                InputEvent::Change => window.refresh(),
                _ => {}
            },
        ));
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
            let locked = form.busy || form.in_doubt.is_some();
            let avatar_supported = app.upgrade().is_some_and(|app| {
                app.read(cx)
                    .caps
                    .as_ref()
                    .is_some_and(|caps| caps.bot_avatar_on_create)
            });
            let stacked = window.viewport_size().width < px(760.);
            let choose = holder.downgrade();
            let studio =
                v_flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .gap_5()
                    .when(stacked, |studio| studio.w_full().pb_2())
                    .when(!stacked, |studio| studio.w(px(248.)))
                    .child(
                        v_flex()
                            .items_center()
                            .gap_2()
                            .child(bot_avatar("create-avatar-preview", form.avatar, px(64.)))
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
            let mut details = v_flex().flex_1().min_w_0().gap_4().child(
                v_flex().gap_1().child(field_label("Bot name", cx)).child(
                    text_field(Input::new(&form.name), cx)
                        .aria_label("Bot name")
                        .disabled(locked),
                ),
            );
            if !categories.is_empty() {
                details = details.child(
                    v_flex().gap_1().child(field_label("Category", cx)).child(
                        field_style(Select::new(&form.category), cx)
                            .focus_ring(false)
                            .cursor_pointer()
                            .accessibility_label("Category")
                            .disabled(locked),
                    ),
                );
            }
            if let Some(profile) = &form.profile {
                details = details.child(div().pt_2().child(profile.clone()));
            }
            if let Some(error) = &form.error {
                details = details.child(
                    div()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(error.clone()),
                );
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
            let busy = form.busy;
            let can_submit = !form.name.read(cx).value().trim().is_empty();
            let app_ok = app.clone();
            let app_close = app.clone();
            let mine = holder.clone();
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
                                .on_click(|_, window, cx| window.close_dialog(cx)),
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
        if holder.read(cx).form.busy || holder.read(cx).form.name.read(cx).value().trim().is_empty()
        {
            return;
        }
        let payload = {
            let form = &holder.read(cx).form;
            form.in_doubt.clone().unwrap_or_else(|| CreateBotRequest {
                request_id: form.request_id,
                name: form.name.read(cx).value().to_string(),
                profile: form.profile.as_ref().and_then(|p| p.read(cx).choice()),
                category_id: form.category.read(cx).selected_value().copied().flatten(),
                avatar: self
                    .caps
                    .as_ref()
                    .is_some_and(|caps| caps.bot_avatar_on_create)
                    .then_some(form.avatar),
            })
        };
        holder.update(cx, |h, cx| {
            h.form.busy = true;
            h.form.error = None;
            if let Some(profile) = &h.form.profile {
                profile.update(cx, |picker, cx| picker.set_disabled(true, cx));
            }
            cx.notify();
        });
        window.refresh();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.create_bot(payload.clone()).await;
            let _ = this.update_in(cx, |app, window, cx| match result {
                Ok(bot) => {
                    if app.create.as_ref() == Some(&holder) {
                        app.create = None;
                        window.close_dialog(cx);
                    }
                    if app.bots.iter().any(|b| b.id == bot.id) {
                        return;
                    }
                    app.bots.push(bot.clone());
                    app.select(Some(bot.id), window, cx);
                    app.composer.read(cx).focus_handle(cx).focus(window, cx);
                    app.refresh_now(window, cx);
                }
                Err(err) => {
                    holder.update(cx, |h, cx| {
                        h.form.busy = false;
                        if err.outcome_unknown() || err.is_request_conflict() {
                            h.form.in_doubt = Some(payload.clone());
                            h.form.error = Some(format!(
                                "{err} Create will retry the original request unchanged."
                            ));
                        } else {
                            h.form.request_id = Uuid::new_v4();
                            h.form.in_doubt = None;
                            h.form.error = Some(err.to_string());
                        }
                        if let Some(profile) = &h.form.profile {
                            profile.update(cx, |picker, cx| {
                                picker.set_disabled(h.form.in_doubt.is_some(), cx);
                            });
                        }
                        cx.notify();
                    });
                    window.refresh();
                }
            });
        })
        .detach();
    }
}

fn field_label(text: &'static str, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}
