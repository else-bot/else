use super::{APP_CONTEXT, Connection, ElseApp, ToggleSettings};
use crate::api::{Bot, BotProfile, ClientError, ProfileChoice};
use crate::profile_picker::{ProfilePicker, effort_label, model_label};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

pub(super) const PANEL_WIDTH: Pixels = px(336.);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Metadata {
    name: String,
}

impl Metadata {
    fn of(bot: &Bot) -> Self {
        Self {
            name: bot.name.clone(),
        }
    }

    fn problem(&self) -> Option<&'static str> {
        let name = self.name.trim();
        if name.is_empty() {
            Some("Give the bot a name.")
        } else if name.chars().count() > 80 {
            Some("Keep the name to 80 characters or fewer.")
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Save {
    Idle,
    Saving(Uuid),
    Saved,
    Failed(String),
}

impl Save {
    fn busy(&self) -> bool {
        matches!(self, Self::Saving(_))
    }
}

struct Details {
    seen: Metadata,
    saved: Metadata,
    name: Entity<InputState>,
    save: Save,
    _edits: Subscription,
}

impl Details {
    fn draft(&self, cx: &App) -> Metadata {
        Metadata {
            name: self.name.read(cx).value().to_string(),
        }
    }

    fn show(&self, values: &Metadata, window: &mut Window, cx: &mut App) {
        let name = values.name.clone();
        self.name
            .update(cx, |state, cx| state.set_value(name, window, cx));
    }
}

enum ModelState {
    Loading(Uuid),
    Failed(String),
    Ready(Option<BotProfile>),
}

pub(super) struct ModelDialog {
    bot: Uuid,
    saved: Option<ProfileChoice>,
    picker: Entity<ProfilePicker>,
    save: Save,
    _picker: Subscription,
}

impl ModelDialog {
    fn dirty(&self, cx: &App) -> bool {
        self.picker.read(cx).choice(cx) != self.saved
    }
}

pub(super) struct BotSettings {
    details: Details,
    model: ModelState,
}

impl ElseApp {
    pub(super) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        if self.settings_open {
            if let Some(bot) = self.selected.filter(|id| {
                self.settings
                    .get(id)
                    .is_some_and(|entry| !matches!(entry.model, ModelState::Loading(_)))
            }) {
                self.load_profile(bot, window, cx);
            }
        } else {
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.ensure_settings(window, cx);
        cx.notify();
    }

    pub(super) fn rename_bot(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active_bots().any(|b| b.id == id) {
            return;
        }
        self.open_bot(id, window, cx);
        if !self.settings_open {
            self.toggle_settings(window, cx);
        }
        if let Some(entry) = self.settings.get(&id) {
            entry.details.name.update(cx, |state, cx| {
                state.focus(window, cx);
                state.select_all(window, cx);
            });
        }
    }

    pub(super) fn settings_shown(&self) -> bool {
        self.settings_open && self.selected_bot().is_some_and(|b| b.is_active())
    }

    pub(super) fn ensure_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_shown() {
            return;
        }
        let Some(bot) = self.selected_bot().cloned() else {
            return;
        };
        if self.settings.contains_key(&bot.id) {
            return;
        }
        let details = new_details(&bot, window, cx);
        self.settings.insert(
            bot.id,
            BotSettings {
                details,
                model: ModelState::Loading(Uuid::nil()),
            },
        );
        self.load_profile(bot.id, window, cx);
    }

    pub(super) fn sync_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bots = &self.bots;
        self.settings
            .retain(|id, _| bots.iter().any(|b| b.id == *id && b.is_active()));
        for bot in &self.bots {
            let Some(details) = self.settings.get_mut(&bot.id).map(|s| &mut s.details) else {
                continue;
            };
            let latest = Metadata::of(bot);
            if latest == details.seen {
                continue;
            }
            if !details.save.busy() {
                let draft = details.draft(cx);
                if draft.name == details.saved.name {
                    details.name.update(cx, |state, cx| {
                        state.set_value(latest.name.clone(), window, cx)
                    });
                }
            }
            details.seen = latest.clone();
            details.saved = latest;
        }
        self.ensure_settings(window, cx);
    }

    fn save_details(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) else {
            return;
        };
        let draft = details.draft(cx);
        if details.save.busy() || draft.problem().is_some() {
            return;
        }
        let attempt = Uuid::new_v4();
        details.save = Save::Saving(attempt);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.update_bot(bot, draft.name.clone()).await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_details(bot, attempt, draft, result, window, cx)
            });
        })
        .detach();
    }

    fn finish_details(
        &mut self,
        bot: Uuid,
        attempt: Uuid,
        sent: Metadata,
        result: Result<Bot, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) else {
            return;
        };
        if details.save != Save::Saving(attempt) {
            return;
        }
        match result {
            Ok(bot) => {
                let saved = Metadata::of(&bot);
                if details.draft(cx) == sent {
                    details.show(&saved, window, cx);
                }
                details.saved = saved;
                details.save = Save::Saved;
            }
            Err(err) => details.save = Save::Failed(err.to_string()),
        }
        cx.notify();
    }

    fn cancel_details(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) {
            details.show(&details.saved, window, cx);
            details.save = Save::Idle;
            cx.notify();
        }
    }

    fn load_profile(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(entry) = self.settings.get_mut(&bot) else {
            return;
        };
        let attempt = Uuid::new_v4();
        entry.model = ModelState::Loading(attempt);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.bot_profile(bot).await;
            let _ = this.update(cx, |app, cx| {
                let Some(entry) = app.settings.get_mut(&bot) else {
                    return;
                };
                if !matches!(entry.model, ModelState::Loading(a) if a == attempt) {
                    return;
                }
                entry.model = match result {
                    Ok(saved) => ModelState::Ready(saved),
                    Err(err) => ModelState::Failed(err.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn open_model_dialog(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(caps) = self.caps.clone() else {
            return;
        };
        let Some(ModelState::Ready(saved)) = self.settings.get(&bot).map(|s| &s.model) else {
            return;
        };
        let saved = saved.as_ref().map(|p| p.choice.clone());
        let picker = cx.new(|cx| ProfilePicker::new(&caps, saved.as_ref(), window, cx).stacked());
        let observer = cx.observe_in(&picker, window, |_, _, window, _| window.refresh());
        let holder = cx.new(|_| ModelDialog {
            bot,
            saved,
            picker: picker.clone(),
            save: Save::Idle,
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
            let ready = form.dirty(cx) && form.picker.read(cx).choice(cx).is_some();
            let body = v_flex()
                .gap_3()
                .child(picker.clone())
                .child(muted(
                    "Changes apply from the bot's next message; work already running is unaffected.",
                    cx,
                ))
                .children(match &form.save {
                    Save::Failed(err) => Some(note(format!("Couldn't apply. {err}"), true, cx)),
                    _ => None,
                });
            let app_ok = app.clone();
            let app_apply = app.clone();
            let app_close = app.clone();
            let mine = holder.clone();
            dialog
                .title("Change model")
                .w(px(420.))
                .on_ok(move |_, window, cx| {
                    let _ = app_ok.update(cx, |app, cx| app.apply_model(window, cx));
                    false
                })
                // Dismissed: a late result must not close another dialog.
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
                                .label("Cancel")
                                .ghost()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("model-apply")
                                .label("Apply")
                                .primary()
                                .loading(saving)
                                .disabled(!ready || saving || !connected)
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
        let (bot, picker) = {
            let form = holder.read(cx);
            (form.bot, form.picker.clone())
        };
        let Some(choice) = picker.read(cx).choice(cx) else {
            return;
        };
        if holder.read(cx).save.busy() || !holder.read(cx).dirty(cx) {
            return;
        }
        let attempt = Uuid::new_v4();
        picker.update(cx, |picker, cx| picker.set_disabled(true, cx));
        holder.update(cx, |form, cx| {
            form.save = Save::Saving(attempt);
            cx.notify();
        });
        window.refresh();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.set_bot_profile(bot, choice).await;
            let _ = this.update_in(cx, |app, window, cx| {
                if holder.read(cx).save != Save::Saving(attempt) {
                    return;
                }
                match result {
                    Ok(saved) => {
                        if let Some(entry) = app.settings.get_mut(&bot)
                            && matches!(entry.model, ModelState::Ready(_))
                        {
                            entry.model = ModelState::Ready(Some(saved));
                        }
                        if app.model_dialog.as_ref() == Some(&holder) {
                            app.model_dialog = None;
                            window.close_dialog(cx);
                        }
                        cx.notify();
                    }
                    Err(err) => {
                        picker.update(cx, |picker, cx| picker.set_disabled(false, cx));
                        holder.update(cx, |form, cx| {
                            form.save = Save::Failed(err.to_string());
                            cx.notify();
                        });
                        window.refresh();
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn render_settings(&self, bot: &Bot, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let header = h_flex()
            .flex_none()
            .px_4()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Bot settings"),
            )
            .child(
                Button::new("close-settings")
                    .icon(IconName::PanelRightClose)
                    .small()
                    .ghost()
                    .tooltip_with_action("Hide settings", &ToggleSettings, Some(APP_CONTEXT))
                    .on_click(cx.listener(|app, _, window, cx| app.toggle_settings(window, cx))),
            );
        let mut body = v_flex()
            .id("bot-settings-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_4()
            .gap_6();
        match self.settings.get(&bot.id) {
            Some(entry) => {
                body = body
                    .child(self.render_details(bot.id, &entry.details, cx))
                    .child(self.render_category(bot, cx))
                    .child(self.render_model(bot.id, &entry.model, cx));
            }
            None => body = body.child(muted("Loading…", cx)),
        }
        v_flex()
            .id("bot-settings")
            .w(PANEL_WIDTH)
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_l_1()
            .border_color(theme.sidebar_border)
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_details(&self, bot: Uuid, details: &Details, cx: &mut Context<Self>) -> AnyElement {
        let draft = details.draft(cx);
        let dirty = draft != details.saved;
        let saving = details.save.busy();
        let problem = draft.problem().filter(|_| dirty);
        let connected = self.connection == Connection::Connected;
        let status = match (&details.save, problem) {
            (_, Some(problem)) => Some(note(problem, true, cx)),
            (Save::Failed(err), _) => Some(note(format!("Couldn't save. {err}"), true, cx)),
            (Save::Saved, _) if !dirty => Some(note("Saved.", false, cx)),
            _ => None,
        };
        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("save-details")
                    .label(if saving { "Saving…" } else { "Save changes" })
                    .small()
                    .primary()
                    .disabled(!dirty || saving || problem.is_some() || !connected)
                    .on_click(
                        cx.listener(move |app, _, window, cx| app.save_details(bot, window, cx)),
                    ),
            )
            .child(
                Button::new("cancel-details")
                    .label("Cancel")
                    .small()
                    .ghost()
                    .disabled(!dirty || saving)
                    .on_click(
                        cx.listener(move |app, _, window, cx| app.cancel_details(bot, window, cx)),
                    ),
            );
        section("Details", cx)
            .child(field("Name", Input::new(&details.name).disabled(saving)))
            .children(status)
            .child(actions)
            .into_any_element()
    }

    fn render_category(&self, bot: &Bot, cx: &mut Context<Self>) -> AnyElement {
        let moving = self.moving.contains(&bot.id);
        let current = bot
            .category_id
            .and_then(|id| self.categories.iter().find(|c| c.id == id))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "Unassigned".to_owned());
        let row = h_flex()
            .gap_2()
            .items_center()
            .child(div().flex_1().min_w_0().text_sm().truncate().child(current))
            .child(
                Button::new("change-category")
                    .label(if moving { "Moving…" } else { "Change…" })
                    .small()
                    .outline()
                    .disabled(moving || self.connection != Connection::Connected)
                    .on_click(cx.listener(|app, _, window, cx| app.open_move_dialog(window, cx))),
            );
        let notice = self
            .category_notice
            .clone()
            .map(|notice| note(notice, true, cx));
        section("Category", cx)
            .child(row)
            .children(notice)
            .child(muted(
                "Groups the bot in the sidebar. Nothing else changes.",
                cx,
            ))
            .into_any_element()
    }

    fn render_model(&self, bot: Uuid, state: &ModelState, cx: &mut Context<Self>) -> AnyElement {
        let body = section("Model", cx);
        let saved = match state {
            ModelState::Loading(_) => return body.child(muted("Loading…", cx)).into_any_element(),
            ModelState::Failed(err) => {
                return body
                    .child(note(format!("Couldn't load the model. {err}"), true, cx))
                    .child(
                        h_flex().child(
                            Button::new("retry-model")
                                .label("Retry")
                                .small()
                                .outline()
                                .on_click(cx.listener(move |app, _, window, cx| {
                                    app.load_profile(bot, window, cx)
                                })),
                        ),
                    )
                    .into_any_element();
            }
            ModelState::Ready(saved) => saved,
        };
        body.child(self.render_current_model(bot, saved.as_ref(), cx))
            .into_any_element()
    }

    fn render_current_model(
        &self,
        bot: Uuid,
        saved: Option<&BotProfile>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match saved {
            None => v_flex().child(muted(
                "No model yet. The bot uses the defaults once they are set.",
                cx,
            )),
            Some(saved) => {
                let choice = &saved.choice;
                let speed = if choice.service_tier == "priority" {
                    "Fast"
                } else {
                    "Standard speed"
                };
                let offered = self.caps.as_ref().is_none_or(|caps| {
                    caps.providers.iter().any(|p| {
                        p.id == choice.provider_id && p.catalog.models.contains(&choice.model)
                    })
                });
                v_flex()
                    .gap_1()
                    .child(muted("Now using", cx))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(model_label(&choice.model)),
                    )
                    .child(div().text_xs().child(format!(
                        "{} · {} reasoning · {speed}",
                        self.provider_label(&choice.provider_id),
                        effort_label(&choice.reasoning_effort),
                    )))
                    .children(
                        (!saved.own).then(|| muted("Follows the defaults for new bots", cx)),
                    )
                    .children((!offered).then(|| {
                        note(
                            "This provider no longer offers the saved model. It stays until you apply another.",
                            true,
                            cx,
                        )
                    }))
            }
        };
        // Button centers and nowraps its content; the card reads as a left-aligned block.
        Button::new("change-model")
            .outline()
            .with_size(Size::Size(px(16.)))
            .w_full()
            .p_3()
            .tooltip("Change model")
            .disabled(self.caps.is_none() || self.connection != Connection::Connected)
            .child(
                content
                    .flex_1()
                    .min_w_0()
                    .items_start()
                    .text_left()
                    .whitespace_normal(),
            )
            .child(Icon::new(IconName::ChevronRight).flex_none())
            .on_click(cx.listener(move |app, _, window, cx| app.open_model_dialog(bot, window, cx)))
            .into_any_element()
    }
}

fn new_details(bot: &Bot, window: &mut Window, cx: &mut Context<ElseApp>) -> Details {
    let values = Metadata::of(bot);
    let name = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("Name")
            .default_value(values.name.clone())
    });
    Details {
        _edits: cx.subscribe(&name, |_, _, _: &InputEvent, cx| cx.notify()),
        seen: values.clone(),
        saved: values,
        name,
        save: Save::Idle,
    }
}

fn section(title: &'static str, cx: &App) -> Div {
    v_flex().gap_3().child(
        div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().muted_foreground)
            .child(title.to_uppercase()),
    )
}

fn field(label: &'static str, input: impl IntoElement) -> Div {
    v_flex()
        .gap_1()
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label))
        .child(input)
}

fn muted(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn note(text: impl Into<SharedString>, error: bool, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .text_xs()
        .text_color(if error { theme.danger } else { theme.success })
        .child(text.into())
}

#[cfg(test)]
mod tests {
    use super::Metadata;

    fn metadata(name: &str) -> Metadata {
        Metadata { name: name.into() }
    }

    #[test]
    fn details_mirror_the_api_limits() {
        assert_eq!(metadata("  Planner ").problem(), None);
        assert_eq!(metadata(&"é".repeat(80)).problem(), None);
        assert!(metadata(" ").problem().is_some());
        assert!(metadata(&"é".repeat(81)).problem().is_some());
    }
}
