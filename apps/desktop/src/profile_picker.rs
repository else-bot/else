//! One catalog-backed profile picker for onboarding and bot dialogs.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, Size, h_flex, v_flex,
};
use gpui_kit::*;

use crate::api::{Capabilities, ProfileChoice, ProviderInfo};

const FAST: &str = "priority";

pub struct ProfilePicker {
    providers: Vec<ProviderInfo>,
    choice: Option<ProfileChoice>,
    search: Entity<InputState>,
    change_focus: FocusHandle,
    browsing: bool,
    customizing: bool,
    disabled: bool,
    _search: Subscription,
}

impl ProfilePicker {
    pub fn new(
        caps: &Capabilities,
        initial: Option<&ProfileChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let providers: Vec<_> = caps
            .providers
            .iter()
            .filter(|p| !p.catalog.models.is_empty() && !p.catalog.efforts.is_empty())
            .cloned()
            .collect();
        let choice = initial
            .and_then(|c| providers.iter().find(|p| p.id == c.provider_id))
            .and_then(|p| catalog_choice(p, initial))
            .or_else(|| {
                providers
                    .iter()
                    .filter(|p| p.cli_available)
                    .find_map(|p| catalog_choice(p, None))
            });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Find a model…"));
        let subscription = cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            browsing: choice.is_none(),
            choice,
            providers,
            search,
            change_focus: cx.focus_handle(),
            customizing: false,
            disabled: false,
            _search: subscription,
        }
    }

    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.disabled != disabled {
            self.disabled = disabled;
            cx.notify();
        }
    }

    pub fn choice(&self) -> Option<ProfileChoice> {
        self.choice.clone()
    }

    pub fn any_available(&self) -> bool {
        self.providers.iter().any(|p| p.cli_available)
    }

    fn current(&self) -> Option<&ProviderInfo> {
        let choice = self.choice.as_ref()?;
        self.providers.iter().find(|p| p.id == choice.provider_id)
    }

    fn choose_model(
        &mut self,
        provider: &str,
        model: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let info = self.providers.iter().find(|p| p.id == provider).unwrap();
        self.choice = catalog_choice(info, self.choice.as_ref()).map(|mut choice| {
            choice.model = model;
            choice
        });
        self.browsing = false;
        self.change_focus.focus(window, cx);
        cx.notify();
    }

    fn render_catalog(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let mut groups = v_flex()
            .id("model-catalog")
            .max_h(px(240.))
            .overflow_y_scroll()
            .p_1();
        let mut found = false;
        for provider in &self.providers {
            let models: Vec<_> = provider
                .catalog
                .models
                .iter()
                .filter(|model| {
                    format!("{} {} {model}", provider.label, model_label(model))
                        .to_lowercase()
                        .contains(&query)
                })
                .collect();
            if models.is_empty() {
                continue;
            }
            found = true;
            groups = groups.child(
                v_flex()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(provider.label.clone()),
                    )
                    .children(provider.cli_unavailable_reason.as_ref().map(|reason| {
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(reason.clone())
                    })),
            );
            for model in models {
                let selected = self.choice.as_ref().is_some_and(|choice| {
                    choice.provider_id == provider.id && choice.model == *model
                });
                let provider_id = provider.id.clone();
                let model_id = model.clone();
                groups = groups.child(
                    Button::new(SharedString::from(format!("model-{provider_id}-{model}")))
                        .cursor_pointer()
                        .ghost()
                        .selected(selected)
                        .toggled(selected)
                        .with_size(Size::Size(px(14.)))
                        .w_full()
                        .px_3()
                        .py_2()
                        .disabled(self.disabled || !provider.cli_available)
                        .accessibility_label(format!("{} · {}", model_label(model), provider.label))
                        .tooltip(model.clone())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_left()
                                .child(model_label(model)),
                        )
                        .child(
                            Icon::new(if selected {
                                IconName::Check
                            } else {
                                IconName::Circle
                            })
                            .size_3()
                            .text_color(if selected {
                                theme.primary
                            } else {
                                theme.border
                            }),
                        )
                        .on_click(cx.listener(move |picker, _, window, cx| {
                            picker.choose_model(&provider_id, model_id.clone(), window, cx);
                        })),
                );
            }
        }
        if !found {
            groups = groups.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No matching models."),
            );
        }
        v_flex()
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius)
            .overflow_hidden()
            .child(
                div().p_2().border_b_1().border_color(theme.border).child(
                    Input::new(&self.search)
                        .small()
                        .prefix(Icon::new(IconName::Search).size_3())
                        .aria_label("Find a model")
                        .disabled(self.disabled),
                ),
            )
            .child(groups)
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(provider) = self.current() else {
            return div().into_any_element();
        };
        let choice = self.choice.as_ref().unwrap();
        let fast_offered = provider.catalog.tiers.iter().any(|tier| tier == FAST);
        let efforts = provider.catalog.efforts.iter().map(|effort| {
            let value = effort.clone();
            let selected = choice.reasoning_effort == *effort;
            Button::new(SharedString::from(format!("effort-{effort}")))
                .cursor_pointer()
                .ghost()
                .small()
                .selected(selected)
                .toggled(selected)
                .label(effort_label(effort).to_owned())
                .accessibility_label(format!("{} reasoning", effort_label(effort)))
                .disabled(self.disabled)
                .on_click(cx.listener(move |picker, _, _, cx| {
                    picker.choice.as_mut().unwrap().reasoning_effort = value.clone();
                    cx.notify();
                }))
        });
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_xs().child("Reasoning"))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_1()
                            .p_1()
                            .bg(theme.muted.opacity(0.3))
                            .rounded(theme.radius)
                            .children(efforts),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Choose how much reasoning the model uses."),
                    ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(div().text_sm().child("Fast mode"))
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                if fast_offered {
                                    "Use priority processing."
                                } else {
                                    "Not available through this provider."
                                },
                            )),
                    )
                    .child(
                        Switch::new("pick-fast")
                            .cursor_pointer()
                            .accessibility_label("Fast mode")
                            .checked(choice.service_tier == FAST)
                            .disabled(self.disabled || !fast_offered)
                            .on_click(cx.listener(|picker, checked: &bool, _, cx| {
                                picker.choice.as_mut().unwrap().service_tier =
                                    if *checked { FAST } else { "default" }.to_owned();
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

fn catalog_choice(
    provider: &ProviderInfo,
    previous: Option<&ProfileChoice>,
) -> Option<ProfileChoice> {
    let previous = previous.filter(|choice| choice.provider_id == provider.id);
    let model = previous
        .map(|choice| &choice.model)
        .filter(|model| provider.catalog.models.contains(model))
        .or(provider.catalog.models.first())?;
    let effort = match previous {
        Some(choice) => provider
            .catalog
            .efforts
            .iter()
            .find(|effort| **effort == choice.reasoning_effort),
        None => provider
            .catalog
            .efforts
            .iter()
            .find(|effort| *effort == "medium"),
    }
    .or(provider.catalog.efforts.first())?;
    let fast = previous.is_some_and(|choice| choice.service_tier == FAST)
        && provider.catalog.tiers.iter().any(|tier| tier == FAST);
    Some(ProfileChoice {
        provider_id: provider.id.clone(),
        model: model.clone(),
        reasoning_effort: effort.clone(),
        service_tier: if fast { FAST } else { "default" }.to_owned(),
    })
}

/// "gpt-6.1-sol" -> "GPT 6.1 Sol", "claude-opus-5-5" -> "Claude Opus 5.5".
pub fn model_label(id: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for part in id.split('-') {
        match words.last_mut() {
            // Version parts join with a dot: 5-5 -> 5.5.
            Some(last)
                if part.chars().all(|c| c.is_ascii_digit())
                    && last.chars().all(|c| c.is_ascii_digit() || c == '.') =>
            {
                last.push('.');
                last.push_str(part);
            }
            _ if part == "gpt" => words.push("GPT".to_owned()),
            _ => {
                let mut chars = part.chars();
                words.push(match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                    None => String::new(),
                });
            }
        }
    }
    words.join(" ")
}

pub fn effort_label(id: &str) -> &str {
    match id {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra high",
        "max" => "Max",
        other => other,
    }
}

impl Render for ProfilePicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let choice = self.choice.as_ref();
        let fast = choice.is_some_and(|choice| choice.service_tier == FAST);
        let chip = |icon, text: String, active| {
            h_flex()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .text_xs()
                .text_color(if active {
                    theme.primary
                } else {
                    theme.muted_foreground
                })
                .child(Icon::new(icon).size_3())
                .child(text)
        };
        let summary = v_flex()
            .p_4()
            .gap_3()
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .bg(theme.muted.opacity(0.2))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size_10()
                            .flex_none()
                            .rounded(theme.radius)
                            .bg(theme.primary.opacity(0.1))
                            .text_color(theme.primary)
                            .child(Icon::new(IconName::Cpu).size_5()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div().text_sm().font_weight(FontWeight::SEMIBOLD).child(
                                    choice
                                        .map(|c| model_label(&c.model))
                                        .unwrap_or_else(|| "Choose a model".into()),
                                ),
                            )
                            .children(self.current().map(|provider| {
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(provider.label.clone())
                            })),
                    )
                    .children(choice.map(|_| {
                        Button::new("pick-change")
                            .cursor_pointer()
                            .link()
                            .small()
                            .label(if self.browsing { "Done" } else { "Change" })
                            .accessibility_label(if self.browsing {
                                "Close model browser"
                            } else {
                                "Change model"
                            })
                            .track_focus(&self.change_focus)
                            .disabled(self.disabled)
                            .on_click(cx.listener(|picker, _, window, cx| {
                                picker.browsing = !picker.browsing;
                                if picker.browsing {
                                    picker
                                        .search
                                        .update(cx, |search, cx| search.set_value("", window, cx));
                                    picker.search.read(cx).focus_handle(cx).focus(window, cx);
                                } else {
                                    picker.change_focus.focus(window, cx);
                                }
                                cx.notify();
                            }))
                    })),
            )
            .children(choice.map(|choice| {
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .child(chip(
                        IconName::SlidersHorizontal,
                        format!("{} reasoning", effort_label(&choice.reasoning_effort)),
                        false,
                    ))
                    .child(chip(
                        if fast { IconName::Zap } else { IconName::Clock },
                        if fast { "Fast mode" } else { "Standard speed" }.to_owned(),
                        fast,
                    ))
            }));
        let mut body = v_flex().gap_3().child(summary);
        if self.browsing {
            body = body.child(self.render_catalog(cx));
        }
        if !self.any_available() {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child("No provider is available on this computer yet."),
            );
        }
        if choice.is_some() {
            body = body.child(
                Button::new("pick-customize")
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label("Customize reasoning & speed")
                    .toggled(self.customizing)
                    .disabled(self.disabled)
                    .child(
                        h_flex()
                            .flex_1()
                            .gap_2()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(IconName::SlidersHorizontal).size_3())
                            .child(
                                div()
                                    .flex_1()
                                    .text_left()
                                    .child("Customize reasoning & speed"),
                            )
                            .child(
                                Icon::new(if self.customizing {
                                    IconName::ChevronUp
                                } else {
                                    IconName::ChevronDown
                                })
                                .size_3(),
                            ),
                    )
                    .on_click(cx.listener(|picker, _, _, cx| {
                        picker.customizing = !picker.customizing;
                        cx.notify();
                    })),
            );
            if self.customizing {
                body = body.child(self.render_settings(cx));
            } else {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("You can change this model and its settings anytime."),
                );
            }
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::{FAST, catalog_choice, model_label};
    use crate::api::{Catalog, ProviderInfo};

    fn provider(id: &str, fast: bool) -> ProviderInfo {
        ProviderInfo {
            id: id.into(),
            label: id.into(),
            cli_available: true,
            cli_unavailable_reason: None,
            catalog: Catalog {
                models: vec!["first".into(), "second".into()],
                efforts: vec!["low".into(), "medium".into(), "high".into()],
                tiers: if fast {
                    vec!["default".into(), FAST.into()]
                } else {
                    vec!["default".into()]
                },
            },
        }
    }

    #[test]
    fn model_ids_read_as_names() {
        assert_eq!(model_label("gpt-6.1-sol"), "GPT 6.1 Sol");
        assert_eq!(model_label("claude-opus-5-5"), "Claude Opus 5.5");
        assert_eq!(model_label("claude-fable-5-1"), "Claude Fable 5.1");
    }

    #[test]
    fn saved_profile_keeps_its_model_reasoning_and_speed() {
        let provider = provider("codex", true);
        let mut saved = catalog_choice(&provider, None).unwrap();
        saved.model = "second".into();
        saved.reasoning_effort = "high".into();
        saved.service_tier = FAST.into();
        assert_eq!(catalog_choice(&provider, Some(&saved)), Some(saved));
    }

    #[test]
    fn switching_provider_starts_at_medium_and_standard_speed() {
        let mut saved = catalog_choice(&provider("codex", true), None).unwrap();
        saved.reasoning_effort = "high".into();
        saved.service_tier = FAST.into();
        let choice = catalog_choice(&provider("claude", false), Some(&saved)).unwrap();
        assert_eq!(choice.provider_id, "claude");
        assert_eq!(choice.reasoning_effort, "medium");
        assert_eq!(choice.service_tier, "default");
    }

    #[test]
    fn removed_catalog_values_fall_back_to_supported_values() {
        let mut saved = catalog_choice(&provider("codex", true), None).unwrap();
        saved.model = "removed".into();
        saved.reasoning_effort = "removed".into();
        saved.service_tier = FAST.into();
        let choice = catalog_choice(&provider("codex", false), Some(&saved)).unwrap();
        assert_eq!(choice.model, "first");
        assert_eq!(choice.reasoning_effort, "low");
        assert_eq!(choice.service_tier, "default");
    }

    #[test]
    fn incomplete_catalog_cannot_produce_a_profile() {
        let mut provider = provider("codex", true);
        provider.catalog.models.clear();
        assert!(catalog_choice(&provider, None).is_none());
        provider.catalog.models.push("first".into());
        provider.catalog.efforts.clear();
        assert!(catalog_choice(&provider, None).is_none());
    }
}
