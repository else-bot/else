use std::collections::HashSet;

use gpui_kit::assets::IconName;
use gpui_kit::base::actions::Cancel;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, Selectable as _, Sizable as _,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::api::{Capabilities, ProfileChoice, ProviderInfo};

const FAST: &str = "priority";
pub(crate) const CATALOG_CONTEXT: &str = "ProfilePickerCatalog";

enum PickerView {
    Settings,
    Catalog { provider: Option<String> },
}

pub struct ProfilePicker {
    providers: Vec<ProviderInfo>,
    choice: Option<ProfileChoice>,
    search: Entity<InputState>,
    model_focus: FocusHandle,
    view: PickerView,
    expanded_providers: HashSet<String>,
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
            view: PickerView::Settings,
            expanded_providers: HashSet::new(),
            choice,
            providers,
            search,
            model_focus: cx.focus_handle(),
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
        self.show_settings(window, cx);
    }

    fn show_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view = PickerView::Settings;
        self.model_focus.focus(window, cx);
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
            if let PickerView::Catalog { provider: Some(id) } = &self.view
                && provider.id != *id
            {
                continue;
            }
            let expanded = self.expanded_providers.contains(&provider.id);
            let models = visible_models(provider, self.choice.as_ref(), &query, expanded);
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
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .selected(selected)
                        .toggled(selected)
                        .small()
                        .w_full()
                        .px_3()
                        .disabled(self.disabled || !provider.cli_available)
                        .accessibility_label(format!("{} · {}", model_label(model), provider.label))
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
            if query.is_empty()
                && !provider.catalog.featured_models.is_empty()
                && provider
                    .catalog
                    .models
                    .iter()
                    .any(|model| !provider.catalog.featured_models.contains(model))
            {
                let provider_id = provider.id.clone();
                groups = groups.child(
                    Button::new(SharedString::from(format!("more-models-{provider_id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .small()
                        .w_full()
                        .label(if expanded {
                            "Fewer models"
                        } else {
                            "More models"
                        })
                        .icon(if expanded {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        })
                        .accessibility_label(format!(
                            "{} {} models",
                            if expanded { "Show fewer" } else { "Show more" },
                            provider.label
                        ))
                        .toggled(expanded)
                        .disabled(self.disabled)
                        .on_click(cx.listener(move |picker, _, _, cx| {
                            if !picker.expanded_providers.remove(&provider_id) {
                                picker.expanded_providers.insert(provider_id.clone());
                            }
                            cx.notify();
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
        let filters = std::iter::once((None, "All".to_owned()))
            .chain(
                self.providers
                    .iter()
                    .map(|p| (Some(p.id.clone()), p.label.clone())),
            )
            .map(|(provider, label)| {
                let selected = matches!(
                    &self.view,
                    PickerView::Catalog { provider: current } if *current == provider
                );
                Button::new(SharedString::from(format!(
                    "provider-{}",
                    provider.as_deref().unwrap_or("all")
                )))
                .focus_ring(false)
                .cursor_pointer()
                .ghost()
                .small()
                .selected(selected)
                .toggled(selected)
                .label(label.clone())
                .accessibility_label(format!("Filter {label} models"))
                .disabled(self.disabled)
                .on_click(cx.listener(move |picker, _, _, cx| {
                    picker.view = PickerView::Catalog {
                        provider: provider.clone(),
                    };
                    cx.notify();
                }))
            });
        v_flex()
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Button::new("model-back")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .icon(IconName::ArrowLeft)
                            .label("Model settings")
                            .accessibility_label("Back to model settings")
                            .disabled(self.disabled)
                            .on_click(cx.listener(|picker, _, window, cx| {
                                picker.show_settings(window, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Choose a model"),
                    ),
            )
            .child(
                Input::new(&self.search)
                    .focus_ring(false)
                    .small()
                    .prefix(Icon::new(IconName::Search).size_3())
                    .aria_label("Find a model")
                    .disabled(self.disabled),
            )
            .child(h_flex().flex_wrap().gap_1().children(filters))
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
                .focus_ring(false)
                .cursor_pointer()
                .ghost()
                .small()
                .selected(selected)
                .toggled(selected)
                .flex_1()
                .flex_basis(px(0.))
                .min_w_0()
                .px_0()
                .label(effort_label(effort).to_owned())
                .accessibility_label(format!("{} reasoning", effort_label(effort)))
                .disabled(self.disabled)
                .on_click(cx.listener(move |picker, _, _, cx| {
                    picker.choice.as_mut().unwrap().reasoning_effort = value.clone();
                    cx.notify();
                }))
        });
        let speeds = [
            ("default", "Standard", IconName::Clock),
            (FAST, "Fast", IconName::Zap),
        ]
        .into_iter()
        .map(|(tier, label, icon)| {
            let selected = choice.service_tier == tier;
            Button::new(SharedString::from(format!("speed-{tier}")))
                .focus_ring(false)
                .cursor_pointer()
                .ghost()
                .small()
                .flex_1()
                .flex_basis(px(0.))
                .min_w_0()
                .selected(selected)
                .toggled(selected)
                .icon(icon)
                .label(label)
                .when(selected, |button| {
                    button
                        .bg(theme.primary.opacity(0.1))
                        .text_color(theme.primary)
                        .border_color(theme.primary.opacity(0.4))
                })
                .accessibility_label(format!("{label} speed"))
                .disabled(self.disabled || (tier == FAST && !fast_offered))
                .on_click(cx.listener(move |picker, _, _, cx| {
                    picker.choice.as_mut().unwrap().service_tier = tier.to_owned();
                    cx.notify();
                }))
        });
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(IconName::SlidersHorizontal).size_3())
                            .child("Reasoning"),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .p_1()
                            .bg(theme.muted.opacity(0.3))
                            .rounded(theme.radius)
                            .children(efforts),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(IconName::Zap).size_3())
                            .child("Speed"),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .p_1()
                            .bg(theme.muted.opacity(0.3))
                            .rounded(theme.radius)
                            .children(speeds),
                    )
                    .when(!fast_offered, |settings| {
                        settings.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Fast mode is not available through this provider."),
                        )
                    }),
            )
            .into_any_element()
    }
}

fn visible_models<'a>(
    provider: &'a ProviderInfo,
    choice: Option<&ProfileChoice>,
    query: &str,
    expanded: bool,
) -> Vec<&'a String> {
    provider
        .catalog
        .models
        .iter()
        .filter(|model| {
            if !query.is_empty() {
                return format!("{} {} {model}", provider.label, model_label(model))
                    .to_lowercase()
                    .contains(query);
            }
            expanded
                || provider.catalog.featured_models.is_empty()
                || provider.catalog.featured_models.contains(model)
                || choice.is_some_and(|choice| {
                    choice.provider_id == provider.id && choice.model == **model
                })
        })
        .collect()
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
        let theme = cx.theme();
        let mut body = v_flex().w_full().gap_4();
        if matches!(self.view, PickerView::Catalog { .. }) {
            return body
                .key_context(CATALOG_CONTEXT)
                .capture_action(cx.listener(|picker, _: &Cancel, window, cx| {
                    picker.show_settings(window, cx);
                    cx.stop_propagation();
                }))
                .child(self.render_catalog(cx));
        }
        let choice = self.choice.as_ref();
        let model = choice
            .map(|c| model_label(&c.model))
            .unwrap_or_else(|| "Choose a model".into());
        body = body.child(
            Button::new("pick-model")
                .focus_ring(false)
                .cursor_pointer()
                .custom(
                    ButtonCustomVariant::new(cx)
                        .color(theme.muted.opacity(0.3))
                        .hover(theme.muted.opacity(0.5))
                        .active(theme.muted.opacity(0.6)),
                )
                .w_full()
                .h_auto()
                .p_3()
                .rounded(theme.radius)
                .track_focus(&self.model_focus)
                .disabled(self.disabled)
                .accessibility_label(format!("Choose model: {model}"))
                .child(
                    h_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size_8()
                                .flex_none()
                                .rounded(theme.radius)
                                .bg(theme.primary.opacity(0.1))
                                .text_color(theme.primary)
                                .child(Icon::new(IconName::Cpu).size_4()),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .text_left()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(model),
                                )
                                .children(self.current().map(|provider| {
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(provider.label.clone())
                                })),
                        )
                        .child(
                            Icon::new(IconName::ChevronRight)
                                .size_4()
                                .text_color(theme.muted_foreground),
                        ),
                )
                .on_click(cx.listener(|picker, _, window, cx| {
                    picker.view = PickerView::Catalog { provider: None };
                    picker.expanded_providers.clear();
                    picker
                        .search
                        .update(cx, |search, cx| search.set_value("", window, cx));
                    picker.search.read(cx).focus_handle(cx).focus(window, cx);
                    cx.notify();
                })),
        );
        if !self.any_available() {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child("No provider is available on this computer yet."),
            );
        }
        if choice.is_some() {
            body = body.child(self.render_settings(cx));
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::{FAST, catalog_choice, model_label, visible_models};
    use crate::api::{Catalog, ProviderInfo};

    fn provider(id: &str, fast: bool) -> ProviderInfo {
        ProviderInfo {
            id: id.into(),
            label: id.into(),
            cli_available: true,
            cli_unavailable_reason: None,
            catalog: Catalog {
                models: vec!["first".into(), "second".into()],
                featured_models: Vec::new(),
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
        let mut provider = provider("codex", true);
        provider.catalog.featured_models = vec!["first".into()];
        let mut saved = catalog_choice(&provider, None).unwrap();
        saved.model = "second".into();
        saved.reasoning_effort = "high".into();
        saved.service_tier = FAST.into();
        assert_eq!(catalog_choice(&provider, Some(&saved)), Some(saved));
    }

    #[test]
    fn more_models_expands_the_catalog_and_search_finds_older_models() {
        let mut provider = provider("codex", true);
        provider.catalog.featured_models = vec!["first".into()];
        assert_eq!(visible_models(&provider, None, "", false), vec!["first"]);
        assert_eq!(
            visible_models(&provider, None, "", true),
            vec!["first", "second"]
        );
        assert_eq!(
            visible_models(&provider, None, "second", false),
            vec!["second"]
        );
        assert!(visible_models(&provider, None, "missing", true).is_empty());
    }

    #[test]
    fn selected_older_model_stays_visible_when_collapsed() {
        let mut provider = provider("codex", true);
        provider.catalog.featured_models = vec!["first".into()];
        let mut saved = catalog_choice(&provider, None).unwrap();
        saved.model = "second".into();
        assert_eq!(
            visible_models(&provider, Some(&saved), "", false),
            vec!["first", "second"]
        );
        saved.provider_id = "claude".into();
        assert_eq!(
            visible_models(&provider, Some(&saved), "", false),
            vec!["first"]
        );
    }

    #[test]
    fn catalogs_without_featured_metadata_show_every_model() {
        let mut provider = provider("claude", false);
        provider.catalog = serde_json::from_value(serde_json::json!({
            "models": ["claude-opus-5-5", "claude-fable-5-1"],
            "efforts": ["medium"],
            "tiers": ["default"]
        }))
        .unwrap();
        assert_eq!(
            visible_models(&provider, None, "", false),
            vec!["claude-opus-5-5", "claude-fable-5-1"]
        );
        assert_eq!(
            visible_models(&provider, None, "fable 5.1", false),
            vec!["claude-fable-5-1"]
        );
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
