use super::super::routines::{RoutineChange, settle_change};
use super::ModelState;
use crate::app::panel::{muted, note};
use crate::app::request::Request;
use crate::app::{Connection, ElseApp};
use crate::controls::{Control as _, Scale, Tier, toggle};
use crate::profile_picker::ProfilePicker;
use else_wire::routines::Routine;
use else_wire::settings::BotProfile;
use else_wire::settings::ProfileChoice;
use gpui_kit::component::button::Button;
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

pub(in crate::app) struct ModelDialog {
    bot: Uuid,
    target: ModelTarget,
    saved: Option<ProfileChoice>,
    picker: Entity<ProfilePicker>,
    save: Request<()>,
    _picker: Subscription,
}

enum ModelTarget {
    Bot,
    Routine {
        id: Uuid,
        name: String,
        use_bot_model: bool,
        unresolved: bool,
    },
}

enum SavedModel {
    Bot(BotProfile),
    Routine(Box<Routine>),
}

enum ModelChange {
    Bot(ProfileChoice),
    Routine {
        id: Uuid,
        choice: Option<ProfileChoice>,
    },
}

impl ModelDialog {
    fn change(&self, cx: &App) -> Option<ModelChange> {
        if self.save.busy() || !self.dirty(cx) {
            return None;
        }
        match self.target {
            ModelTarget::Bot => self.choice(cx).map(ModelChange::Bot),
            ModelTarget::Routine { id, .. } if self.inherits() => {
                Some(ModelChange::Routine { id, choice: None })
            }
            ModelTarget::Routine { id, .. } => self.choice(cx).map(|choice| ModelChange::Routine {
                id,
                choice: Some(choice),
            }),
        }
    }

    fn routine_id(&self) -> Option<Uuid> {
        match self.target {
            ModelTarget::Bot => None,
            ModelTarget::Routine { id, .. } => Some(id),
        }
    }

    fn choice(&self, cx: &App) -> Option<ProfileChoice> {
        if self.inherits() {
            None
        } else {
            self.picker.read(cx).choice()
        }
    }

    fn inherits(&self) -> bool {
        matches!(
            self.target,
            ModelTarget::Routine {
                use_bot_model: true,
                ..
            }
        )
    }

    fn dirty(&self, cx: &App) -> bool {
        self.choice(cx) != self.saved
            || matches!(
                self.target,
                ModelTarget::Routine {
                    unresolved: true,
                    ..
                }
            )
    }
}

impl ElseApp {
    pub(super) fn open_model_dialog(
        &mut self,
        bot: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.settings.get(&bot).and_then(|s| s.model.shown()) else {
            return;
        };
        let saved = saved.as_ref().map(|p| p.choice.clone());
        self.open_model_editor(bot, ModelTarget::Bot, saved, window, cx);
    }

    /// Opens the bot's model picker with providers and the saved model read
    /// again, so a provider that just became unavailable shows why.
    pub(in crate::app) fn switch_model(
        &mut self,
        bot: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.details.open() {
            self.toggle_settings(window, cx);
        }
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (caps, saved) =
                futures_util::join!(client.clone().capabilities(), client.bot_profile(bot));
            let _ = this.update_in(cx, |app, window, cx| {
                if caps.is_ok() {
                    app.apply_caps(caps, window, cx);
                }
                if app.selected != Some(bot) {
                    return;
                }
                if let (Ok(saved), Some(entry)) = (saved, app.settings.get_mut(&bot)) {
                    entry.model = ModelState::Ready(saved);
                }
                app.open_model_dialog(bot, window, cx);
            });
        })
        .detach();
    }

    pub(in crate::app::settings) fn open_routine_model_dialog(
        &mut self,
        bot: Uuid,
        routine: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(routine) = self.changeable_routine(bot, routine) else {
            return;
        };
        let target = ModelTarget::Routine {
            id: routine.id,
            name: routine.name.clone(),
            use_bot_model: routine.model.is_none(),
            unresolved: routine.requested_model.is_some(),
        };
        let saved = routine.model.clone();
        self.open_model_editor(bot, target, saved, window, cx);
    }

    fn open_model_editor(
        &mut self,
        bot: Uuid,
        target: ModelTarget,
        saved: Option<ProfileChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(caps) = self.caps.clone() else {
            return;
        };
        let initial = saved.as_ref().or_else(|| {
            self.settings
                .get(&bot)
                .and_then(|s| s.model.shown()?.as_ref())
                .map(|profile| &profile.choice)
        });
        let initial = initial.cloned();
        let picker = self.profile_picker(&caps, initial.as_ref(), cx);
        let picker = cx.new(|_| picker);
        let observer = cx.observe_in(&picker, window, |_, _, window, _| window.refresh());
        let holder = cx.new(|_| ModelDialog {
            bot,
            target,
            saved,
            picker: picker.clone(),
            save: Request::Idle,
            _picker: observer,
        });
        self.model_dialog = Some(holder.clone());
        let app = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let form = holder.read(cx);
            let saving = form.save.busy();
            let connected = app
                .upgrade()
                .is_some_and(|app| app.read(cx).connection == Connection::Connected);
            let ready = form.change(cx).is_some();
            let routine = form.routine_id().is_some();
            let inherit_holder = holder.clone();
            let inheritance = routine.then(|| {
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        v_flex().flex_1().gap_1()
                            .child(div().text_sm().child("Use bot model"))
                            .child(muted("Follow the bot's model, including future changes.", cx)),
                    )
                    .child(
                        toggle("routine-inherit-model", form.inherits(), saving || !connected, cx)
                            .accessibility_label("Use bot model")
                            .on_click(move |enabled: &bool, window, cx| {
                                inherit_holder.update(cx, |form, cx| {
                                    if let ModelTarget::Routine { use_bot_model, .. } = &mut form.target {
                                        *use_bot_model = *enabled;
                                        cx.notify();
                                    }
                                });
                                window.refresh();
                            }),
                    )
            });
            let body = v_flex()
                .gap_3()
                .children(match &form.target {
                    ModelTarget::Routine { name, .. } => Some(div().text_sm().truncate().child(name.clone())),
                    ModelTarget::Bot => None,
                })
                .children(inheritance)
                .when(!form.inherits(), |body| body.child(picker.clone()))
                .child(muted(
                    if routine {
                        "Applies to future runs. Runs already queued or running keep their model."
                    } else {
                        "Changes apply from the bot's next message; work already running is unaffected."
                    },
                    cx,
                ))
                .children(
                    form.save
                        .error()
                        .map(|err| note(format!("Couldn't apply. {err}"), true, cx)),
                );
            let app_ok = app.clone();
            let app_apply = app.clone();
            let app_close = app.clone();
            let mine = holder.clone();
            dialog
                .title(if routine { "Routine model" } else { "Change model" })
                .w(px(640.))
                .on_ok(move |_, window, cx| {
                    let _ = app_ok.update(cx, |app, cx| app.apply_model(window, cx));
                    false
                })
                .on_close(move |_, _, cx| {
                    let _ = app_close.update(cx, |app, _| {
                        if app.model_dialog.as_ref() == Some(&mine) {
                            app.model_dialog = None;
                        }
                    });
                })
                .child(body)
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("model-cancel")
                                .control(Tier::Secondary, Scale::Standard, false, cx)
                                .label("Cancel")
                                                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("model-apply")
                                .label("Apply")
                                .control(Tier::Primary, Scale::Standard, !ready || saving || !connected, cx)
                                .loading(saving)
                                .on_click(move |_, window, cx| {
                                    let _ = app_apply.update(cx, |app, cx| app.apply_model(window, cx));
                                }),
                        ),
                )
        });
    }

    fn apply_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (client, Some(holder)) = (self.client.clone(), self.model_dialog.clone()) else {
            return;
        };
        let (bot, routine, picker, change) = {
            let form = holder.read(cx);
            let Some(change) = form.change(cx) else {
                return;
            };
            (form.bot, form.routine_id(), form.picker.clone(), change)
        };
        if holder.read(cx).save.busy() {
            return;
        }
        let routine_attempt = match routine {
            Some(routine) => {
                let Some(attempt) = self.begin_routine_change(bot, routine, RoutineChange::Model)
                else {
                    holder.update(cx, |form, cx| {
                        form.save.reject("This routine can't be changed right now. Reopen its model settings and try again.");
                        cx.notify();
                    });
                    window.refresh();
                    return;
                };
                Some((routine, attempt))
            }
            None => None,
        };
        picker.update(cx, |picker, cx| picker.set_disabled(true, cx));
        let Some(attempt) = holder.update(cx, |form, cx| {
            cx.notify();
            form.save.begin(())
        }) else {
            return;
        };
        window.refresh();
        cx.spawn_in(window, async move |this, cx| {
            let result = match change {
                ModelChange::Routine { id, choice } => client
                    .set_routine_model(bot, id, choice)
                    .await
                    .map(|saved| SavedModel::Routine(Box::new(saved))),
                ModelChange::Bot(choice) => client
                    .set_bot_profile(bot, choice)
                    .await
                    .map(SavedModel::Bot),
            };
            let _ = this.update_in(cx, |app, window, cx| {
                if let Err(err) = &result {
                    app.unauthorized(err, window, cx);
                }
                if let Some((routine, routine_attempt)) = routine_attempt
                    && settle_change(
                        &mut app.settings,
                        bot,
                        routine,
                        routine_attempt,
                        &result,
                        |err| format!("Couldn't save. {err}"),
                    )
                    && result.is_ok()
                    && let Some(entry) = app.settings.get_mut(&bot)
                {
                    entry.routines.remove(&routine);
                }
                if !holder.update(cx, |form, _| form.save.settle(attempt, &result)) {
                    return;
                }
                match result {
                    Ok(saved) => {
                        match saved {
                            SavedModel::Bot(saved) => {
                                if let Some(entry) = app.settings.get_mut(&bot) {
                                    entry.model.saved(saved);
                                }
                            }
                            SavedModel::Routine(saved) => {
                                app.apply_routine(bot, *saved);
                            }
                        }
                        if app.model_dialog.as_ref() == Some(&holder) {
                            app.model_dialog = None;
                            window.close_dialog(cx);
                        }
                        cx.notify();
                    }
                    Err(_) => {
                        picker.update(cx, |picker, cx| picker.set_disabled(false, cx));
                        holder.update(cx, |_, cx| cx.notify());
                        window.refresh();
                    }
                }
            });
        })
        .detach();
    }
}
