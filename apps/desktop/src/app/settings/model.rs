mod editor;

pub(in crate::app) use editor::ModelDialog;

use crate::app::panel::{alert, item_name, meta, muted, note, section};
use crate::app::request::Request;
use crate::app::{Connection, ElseApp};
use crate::controls::{Control as _, Scale, Tier};
use crate::profile_picker::{ProfilePicker, effort_label};
use else_wire::settings::{BotProfile, Capabilities, ModelInfo, ProfileChoice, model_label};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, IconName, Sizable as _, Size,
    h_flex, v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

/// How far a clickable row's hover fill reaches past the text column.
const ROW_BLEED: Pixels = px(8.);

pub(super) enum ModelState {
    Loading(Uuid),
    Failed(String),
    Ready(Option<BotProfile>),
    /// Every routine, trigger and turn change moves the bot's revision and
    /// reloads its profile, so a reload keeps the shown profile on screen
    /// and the panel's sections don't jump.
    Refreshing {
        attempt: Uuid,
        shown: Option<BotProfile>,
    },
}

impl ModelState {
    pub(super) fn shown(&self) -> Option<&Option<BotProfile>> {
        match self {
            Self::Ready(shown) | Self::Refreshing { shown, .. } => Some(shown),
            Self::Loading(_) | Self::Failed(_) => None,
        }
    }

    pub(super) fn pending(&self) -> bool {
        matches!(self, Self::Loading(_) | Self::Refreshing { .. })
    }

    fn reload(&mut self, attempt: Uuid) {
        *self = match std::mem::replace(self, Self::Loading(attempt)) {
            Self::Ready(shown) | Self::Refreshing { shown, .. } => {
                Self::Refreshing { attempt, shown }
            }
            Self::Loading(_) | Self::Failed(_) => Self::Loading(attempt),
        };
    }

    /// Applies the answer to `attempt`, unless a later load superseded it.
    /// A failed refresh keeps the shown profile; the next revision retries.
    fn answer(&mut self, attempt: Uuid, result: Result<Option<BotProfile>, String>) -> bool {
        let next = match (&*self, result) {
            (Self::Loading(a) | Self::Refreshing { attempt: a, .. }, Ok(profile))
                if *a == attempt =>
            {
                Self::Ready(profile)
            }
            (Self::Loading(a), Err(err)) if *a == attempt => Self::Failed(err),
            (Self::Refreshing { attempt: a, shown }, Err(_)) if *a == attempt => {
                Self::Ready(shown.clone())
            }
            _ => return false,
        };
        *self = next;
        true
    }

    /// A saved profile is current, but a pending load stays pending so a
    /// change made elsewhere after the save still lands.
    fn saved(&mut self, profile: BotProfile) {
        match self {
            Self::Ready(_) | Self::Failed(_) => *self = Self::Ready(Some(profile)),
            Self::Refreshing { shown, .. } => *shown = Some(profile),
            Self::Loading(_) => {}
        }
    }
}

impl ElseApp {
    pub(super) fn load_profile(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(entry) = self.settings.get_mut(&bot) else {
            return;
        };
        let attempt = Uuid::new_v4();
        entry.model.reload(attempt);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.bot_profile(bot).await;
            let _ = this.update(cx, |app, cx| {
                let Some(entry) = app.settings.get_mut(&bot) else {
                    return;
                };
                if entry
                    .model
                    .answer(attempt, result.map_err(|err| err.to_string()))
                {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_model(
        &self,
        bot: Uuid,
        state: &ModelState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = section("Model", cx);
        let Some(saved) = state.shown() else {
            let ModelState::Failed(err) = state else {
                return body.child(muted("Loading…", cx)).into_any_element();
            };
            return body
                .child(note(format!("Couldn't load the model. {err}"), true, cx))
                .child(
                    h_flex().child(
                        Button::new("retry-model")
                            .control(Tier::Secondary, Scale::Standard, false, cx)
                            .label("Retry")
                            .on_click(cx.listener(move |app, _, window, cx| {
                                app.load_profile(bot, window, cx)
                            })),
                    ),
                )
                .into_any_element();
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
            None => v_flex().child(muted("No model yet. Choose a bot model.", cx)),
            Some(saved) => {
                let choice = &saved.choice;
                let speed = if choice.service_tier == "priority" {
                    "Fast"
                } else {
                    "Standard speed"
                };
                v_flex()
                    .gap_0p5()
                    .child(item_name(self.model_name(choice)))
                    .child(meta(
                        format!(
                            "{} · {} reasoning · {speed}",
                            self.provider_label(&choice.provider_id),
                            effort_label(&choice.reasoning_effort),
                        ),
                        cx,
                    ))
            }
        };
        let upgrade = saved.and_then(|saved| self.render_model_upgrade(bot, &saved.choice, cx));
        // The warning sits under the button: a button keeps its label on one
        // line, which would clip the sentence.
        let unavailable = saved
            .is_some_and(|saved| !self.model_available(&saved.choice))
            .then(|| {
                alert(
                    "Saved model is unavailable. Choose another model.",
                    cx.theme().danger,
                    cx,
                )
            });
        // Button centers and nowraps its content; the card reads as a left-aligned block.
        let button = Button::new("change-model")
            .focus_ring(false)
            .cursor_pointer()
            .ghost()
            .with_size(Size::Size(px(16.)))
            .mx(-ROW_BLEED)
            .my(-ROW_BLEED)
            .px(ROW_BLEED)
            .py(ROW_BLEED)
            .accessibility_label("Change model")
            .disabled(self.caps.is_none() || self.connection != Connection::Connected)
            .child(
                content
                    .flex_1()
                    .min_w_0()
                    .items_start()
                    .text_left()
                    .whitespace_normal(),
            )
            .child(
                Icon::new(IconName::ChevronRight)
                    .flex_none()
                    .text_color(cx.theme().muted_foreground),
            )
            .on_click(
                cx.listener(move |app, _, window, cx| app.open_model_dialog(bot, window, cx)),
            );
        v_flex()
            .w_full()
            .child(button)
            .children(unavailable)
            .children(upgrade)
            .into_any_element()
    }

    /// The catalog entry of a saved choice, when its provider lists it.
    fn model_info(&self, choice: &ProfileChoice) -> Option<&ModelInfo> {
        self.caps
            .as_ref()?
            .providers
            .iter()
            .find(|provider| provider.state.id == choice.provider_id)?
            .catalog
            .model(&choice.model)
    }

    /// The provider's name for a model, else one read from its ID.
    pub(in crate::app) fn model_name(&self, choice: &ProfileChoice) -> String {
        self.model_info(choice)
            .map_or_else(|| model_label(&choice.model), |model| model.label.clone())
    }

    /// "Claude Fable 5.2 available" with a Switch button, when the saved
    /// model's family has a newer model. Never switches on its own.
    fn render_model_upgrade(
        &self,
        bot: Uuid,
        choice: &ProfileChoice,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let newer = self.model_info(choice)?.upgrade.clone()?;
        let target = ProfileChoice {
            model: newer,
            ..choice.clone()
        };
        let label = self.model_info(&target)?.label.clone();
        let upgrade = &self.settings.get(&bot)?.model_upgrade;
        let saving = upgrade.busy();
        let error = upgrade.error().map(str::to_owned);
        let connected = self.connection == Connection::Connected;
        Some(
            v_flex()
                .w_full()
                .gap_1()
                .pt_2()
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            Icon::new(IconName::ArrowUp)
                                .size_3p5()
                                .flex_none()
                                .text_color(cx.theme().primary),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .child(format!("{label} available")),
                        )
                        .child(
                            Button::new("upgrade-model")
                                .control(Tier::Secondary, Scale::Compact, !connected || saving, cx)
                                .label("Switch")
                                .loading(saving)
                                .accessibility_label(format!("Switch to {label}"))
                                .on_click(cx.listener(move |app, _, window, cx| {
                                    app.upgrade_model(bot, target.clone(), window, cx)
                                })),
                        ),
                )
                .children(error.map(|error| {
                    alert(format!("Couldn't switch. {error}"), cx.theme().danger, cx)
                })),
        )
    }

    /// Save `target`, the newer model with the bot's effort and speed where
    /// it takes them.
    fn upgrade_model(
        &mut self,
        bot: Uuid,
        target: ProfileChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.caps.as_ref().and_then(|caps| {
            caps.providers
                .iter()
                .find(|p| p.state.id == target.provider_id)
        }) else {
            return;
        };
        let Some(choice) = crate::profile_picker::catalog_choice(provider, Some(&target)) else {
            return;
        };
        let Some(attempt) = self
            .settings
            .get_mut(&bot)
            .and_then(|entry| entry.model_upgrade.begin(()))
        else {
            return;
        };
        cx.notify();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.set_bot_profile(bot, choice).await;
            let _ = this.update_in(cx, |app, window, cx| {
                if let Err(err) = &result {
                    app.unauthorized(err, window, cx);
                }
                let Some(entry) = app.settings.get_mut(&bot) else {
                    return;
                };
                if !entry.model_upgrade.settle(attempt, &result) {
                    return;
                }
                if let Ok(saved) = result {
                    entry.model.saved(saved);
                    entry.model_upgrade = Request::Idle;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A model picker that badges models listed since the owner last
    /// opened one.
    pub(in crate::app) fn profile_picker(
        &mut self,
        caps: &Capabilities,
        initial: Option<&ProfileChoice>,
        cx: &mut Context<Self>,
    ) -> ProfilePicker {
        let new = self.preferences.see_models(caps);
        self.save_preferences(cx);
        ProfilePicker::new(caps, initial).with_new(new)
    }

    pub(super) fn model_available(&self, choice: &ProfileChoice) -> bool {
        self.caps.as_ref().is_none_or(|caps| {
            caps.providers.iter().any(|provider| {
                provider.state.id == choice.provider_id
                    && provider.state.cli_available
                    && provider.catalog.applies(choice)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ModelState;
    use else_wire::settings::{BotProfile, ProfileChoice};
    use uuid::Uuid;

    fn profile(model: &str) -> Option<BotProfile> {
        Some(BotProfile {
            choice: ProfileChoice {
                provider_id: "codex".into(),
                model: model.into(),
                reasoning_effort: "medium".into(),
                service_tier: "priority".into(),
            },
            own: true,
            revision: Some(1),
        })
    }

    #[test]
    fn a_refresh_shows_the_profile_until_its_answer_lands() {
        let mut state = ModelState::Ready(profile("gpt-6.1-sol"));
        let attempt = Uuid::new_v4();
        state.reload(attempt);
        assert_eq!(state.shown(), Some(&profile("gpt-6.1-sol")));
        assert!(state.answer(attempt, Ok(profile("gpt-6.2-sol"))));
        assert_eq!(state.shown(), Some(&profile("gpt-6.2-sol")));
    }

    #[test]
    fn a_superseded_answer_is_dropped() {
        let mut state = ModelState::Ready(profile("gpt-6.1-sol"));
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        state.reload(first);
        state.reload(second);
        assert!(state.answer(second, Ok(profile("gpt-6.2-sol"))));
        assert!(!state.answer(first, Ok(profile("gpt-6.1-sol"))));
        assert_eq!(state.shown(), Some(&profile("gpt-6.2-sol")));
    }

    #[test]
    fn a_failed_refresh_keeps_the_profile_and_a_failed_first_load_shows_the_error() {
        let mut state = ModelState::Ready(profile("gpt-6.1-sol"));
        let attempt = Uuid::new_v4();
        state.reload(attempt);
        assert!(state.answer(attempt, Err("offline".into())));
        assert_eq!(state.shown(), Some(&profile("gpt-6.1-sol")));

        let mut state = ModelState::Loading(Uuid::nil());
        let attempt = Uuid::new_v4();
        state.reload(attempt);
        assert_eq!(state.shown(), None);
        assert!(state.answer(attempt, Err("offline".into())));
        assert!(matches!(&state, ModelState::Failed(err) if err == "offline"));
    }

    #[test]
    fn a_save_during_a_refresh_shows_at_once_and_the_refresh_still_lands() {
        let mut state = ModelState::Ready(profile("gpt-6.1-sol"));
        let attempt = Uuid::new_v4();
        state.reload(attempt);
        state.saved(profile("gpt-6.2-sol").unwrap());
        assert_eq!(state.shown(), Some(&profile("gpt-6.2-sol")));
        assert!(state.answer(attempt, Ok(profile("gpt-6.3-sol"))));
        assert_eq!(state.shown(), Some(&profile("gpt-6.3-sol")));
    }
}
