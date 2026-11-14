use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, Selectable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::api::{Capabilities, ProfileChoice, ProviderInfo};
use crate::controls::{Control as _, Scale, Tier, segment, segment_track};

const FAST: &str = "priority";

pub struct ProfilePicker {
    providers: Vec<ProviderInfo>,
    choice: Option<ProfileChoice>,
    browsing: Option<String>,
    copied: Option<String>,
    /// The provider whose full setup problem is expanded.
    explained: Option<String>,
    tabs: bool,
    disabled: bool,
}

impl ProfilePicker {
    pub fn new(caps: &Capabilities, initial: Option<&ProfileChoice>) -> Self {
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
        let browsing = choice
            .as_ref()
            .map(|c| c.provider_id.clone())
            .or_else(|| providers.first().map(|p| p.id.clone()));
        Self {
            providers,
            choice,
            browsing,
            copied: None,
            explained: None,
            tabs: false,
            disabled: false,
        }
    }

    /// Lists providers as a row of tabs above the models, for narrow hosts.
    pub fn tabs(mut self) -> Self {
        self.tabs = true;
        self
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

    fn browsed(&self) -> Option<&ProviderInfo> {
        let id = self.browsing.as_ref()?;
        self.providers.iter().find(|p| p.id == *id)
    }

    fn choose_model(&mut self, provider: &str, model: String, cx: &mut Context<Self>) {
        let Some(info) = self.providers.iter().find(|p| p.id == provider) else {
            return;
        };
        self.choice = catalog_choice(info, self.choice.as_ref()).map(|mut choice| {
            choice.model = model;
            choice
        });
        cx.notify();
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let items = self.providers.iter().map(|provider| {
            let browsing = self.browsing.as_deref() == Some(provider.id.as_str());
            let holds_choice = self
                .choice
                .as_ref()
                .is_some_and(|choice| choice.provider_id == provider.id);
            let status = if provider.cli_available {
                match provider.catalog.models.len() {
                    1 => "1 model".to_owned(),
                    n => format!("{n} models"),
                }
            } else {
                "Needs setup".to_owned()
            };
            let health = if provider.cli_available {
                theme.success
            } else {
                theme.danger
            };
            let dot = || div().flex_none().size_1p5().rounded_full().bg(health);
            let id = provider.id.clone();
            let select = cx.listener(move |picker, _, _, cx| {
                picker.browsing = Some(id.clone());
                cx.notify();
            });
            let button_id = SharedString::from(format!("provider-{}", provider.id));
            if self.tabs {
                return segment(
                    button_id,
                    provider.label.clone(),
                    browsing,
                    self.disabled,
                    cx,
                )
                .accessibility_label(format!("{} · {status}", provider.label))
                .on_click(select);
            }
            Button::new(button_id)
                .focus_ring(false)
                .ghost()
                .selected(browsing)
                .disabled(self.disabled)
                .when(!self.disabled, |button| button.cursor_pointer())
                .w_full()
                .h_auto()
                .px_2()
                .py_1p5()
                .accessibility_label(format!("{} · {status}", provider.label))
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .items_start()
                                .child(
                                    h_flex().w_full().min_w_0().gap_1p5().child(
                                        div()
                                            .min_w_0()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(theme.foreground)
                                            .truncate()
                                            .child(provider.label.clone()),
                                    ),
                                )
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(dot())
                                        .child(status),
                                ),
                        )
                        .children(holds_choice.then(|| {
                            Icon::new(IconName::Check)
                                .size_3p5()
                                .text_color(theme.primary)
                        })),
                )
                .on_click(select)
        });
        if self.tabs {
            // A form field: equal segments fill the field column, edge to
            // edge with the inputs above.
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child("Provider"),
                )
                .child(segment_track(cx).w_full().children(items))
                .into_any_element()
        } else {
            v_flex()
                .flex_none()
                .w(px(156.))
                .gap_1()
                .p_1p5()
                .border_r_1()
                .border_color(theme.border)
                .bg(theme.muted.opacity(0.3))
                .children(items)
                .into_any_element()
        }
    }

    fn render_models(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let pane = v_flex()
            .id("model-catalog")
            .flex_1()
            .min_w_0()
            .map(|pane| {
                if self.tabs {
                    pane.max_h(px(320.))
                } else {
                    pane.h(px(300.))
                }
            })
            .overflow_y_scroll()
            .map(|pane| if self.tabs { pane.py_2() } else { pane.p_2() })
            .gap_3();
        let Some(provider) = self.browsed() else {
            return pane.into_any_element();
        };
        if !provider.cli_available {
            return pane
                .child(self.render_setup(provider, cx))
                .into_any_element();
        }
        let (featured, older) = split_models(provider);
        let selected = |model: &str| {
            self.choice
                .as_ref()
                .is_some_and(|choice| choice.provider_id == provider.id && choice.model == model)
        };
        let cards = featured.into_iter().map(|model| {
            let on = selected(model);
            let provider_id = provider.id.clone();
            let model_id = model.to_owned();
            div().w(relative(0.5)).p_1().child(
                Button::new(SharedString::from(format!("model-{}-{model}", provider.id)))
                    .focus_ring(false)
                    .ghost()
                    .selected(on)
                    .disabled(self.disabled)
                    .when(!self.disabled, |button| button.cursor_pointer())
                    .w_full()
                    .h_auto()
                    .p_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(if on { theme.primary } else { theme.border })
                    .accessibility_label(format!("{} · {}", model_label(model), provider.label))
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(theme.foreground)
                                    .child(model.to_owned()),
                            )
                            .child(radio(on, cx)),
                    )
                    .on_click(cx.listener(move |picker, _, _, cx| {
                        picker.choose_model(&provider_id, model_id.clone(), cx);
                    })),
            )
        });
        let older_rows = older.iter().enumerate().map(|(index, model)| {
            let on = selected(model);
            let provider_id = provider.id.clone();
            let model_id = (*model).to_owned();
            Button::new(SharedString::from(format!("model-{}-{model}", provider.id)))
                .focus_ring(false)
                .ghost()
                .selected(on)
                .disabled(self.disabled)
                .when(!self.disabled, |button| button.cursor_pointer())
                .w_full()
                .h_9()
                .px_3()
                .rounded_none()
                .when(index > 0, |row| row.border_t_1().border_color(theme.border))
                .accessibility_label(format!("{} · {}", model_label(model), provider.label))
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.foreground)
                                .child((*model).to_owned()),
                        )
                        .children(on.then(|| {
                            Icon::new(IconName::Check)
                                .size_3p5()
                                .text_color(theme.primary)
                        })),
                )
                .on_click(cx.listener(move |picker, _, _, cx| {
                    picker.choose_model(&provider_id, model_id.clone(), cx);
                }))
        });
        pane.child(h_flex().w_full().flex_wrap().children(cards))
            .when(!older.is_empty(), |pane| {
                pane.child(
                    v_flex()
                        .px_1()
                        .gap_1p5()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.muted_foreground)
                                .child("Older models"),
                        )
                        .child(
                            v_flex()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.border)
                                .overflow_hidden()
                                .children(older_rows),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_setup(&self, provider: &ProviderInfo, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let reason = provider
            .cli_unavailable_reason
            .clone()
            .unwrap_or_else(|| format!("{} is not available on this computer.", provider.label));
        let command = setup_command(&reason).map(str::to_owned);
        let copied = self.copied.as_deref() == Some(provider.id.as_str());
        let explained = self.explained.as_deref() == Some(provider.id.as_str());
        let explain_id = provider.id.clone();
        // Under the Provider tabs the warning row is the whole notice; the
        // side-by-side catalog keeps a heading over it.
        v_flex()
            .map(|setup| if self.tabs { setup } else { setup.p_3() })
            .gap_2()
            .when(!self.tabs, |setup| {
                setup.child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(format!("Connect {}", provider.label)),
                )
            })
            // Why the provider can't run: one short warning line, with the
            // server's full reason behind a disclosure.
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .line_height(px(18.))
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .size_3p5()
                            .flex_none()
                            .text_color(theme.warning),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.warning)
                            .child(format!("{} isn't available yet.", provider.label)),
                    )
                    .child(
                        Button::new(SharedString::from(format!("setup-details-{}", provider.id)))
                            .control(Tier::Ghost, Scale::Compact, false, cx)
                            // 13 px, like the warning it explains.
                            .child(div().text_size(px(13.)).child(if explained {
                                "Hide details"
                            } else {
                                "Details"
                            }))
                            .accessibility_label(if explained { "Hide details" } else { "Details" })
                            .icon(if explained {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .on_click(cx.listener(move |picker, _, _, cx| {
                                picker.explained = if explained {
                                    None
                                } else {
                                    Some(explain_id.clone())
                                };
                                cx.notify();
                            })),
                    ),
            )
            .when(explained, |setup| {
                setup.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .whitespace_normal()
                        .child(reason),
                )
            })
            .children(command.map(|command| {
                let provider_id = provider.id.clone();
                let text = command.clone();
                h_flex()
                    .gap_2()
                    .pl_3()
                    .pr_1()
                    .py_1()
                    .rounded(theme.radius)
                    .bg(theme.muted.opacity(0.5))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .child(format!("$ {command}")),
                    )
                    .child(
                        Button::new(SharedString::from(format!("copy-setup-{}", provider.id)))
                            .control(Tier::Ghost, Scale::Compact, self.disabled, cx)
                            .icon(if copied {
                                IconName::Check
                            } else {
                                IconName::Copy
                            })
                            .label(if copied { "Copied" } else { "Copy" })
                            .accessibility_label(format!("Copy {command}"))
                            .on_click(cx.listener(move |picker, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                picker.copied = Some(provider_id.clone());
                                cx.notify();
                            })),
                    )
            }))
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (Some(provider), Some(choice)) = (self.current(), self.choice.as_ref()) else {
            return div().into_any_element();
        };
        let fast_offered = provider.catalog.tiers.iter().any(|tier| tier == FAST);
        let efforts = &provider.catalog.efforts;
        let index = efforts
            .iter()
            .position(|effort| *effort == choice.reasoning_effort)
            .unwrap_or(0);
        // Equal segments fill each field: stacked under the provider tabs
        // or side by side beside the catalog.
        let slider = segment_track(cx)
            .w_full()
            .children(efforts.iter().enumerate().map(|(i, effort)| {
                let value = effort.clone();
                let on = i == index;
                segment(
                    SharedString::from(format!("effort-{effort}")),
                    short_effort_label(effort).to_owned(),
                    on,
                    self.disabled,
                    cx,
                )
                .toggled(on)
                .accessibility_label(format!("{} reasoning", effort_label(effort)))
                .on_click(cx.listener(move |picker, _, _, cx| {
                    if let Some(choice) = picker.choice.as_mut() {
                        choice.reasoning_effort = value.clone();
                    }
                    cx.notify();
                }))
            }));
        let speeds = [
            ("default", "Standard", IconName::Clock),
            (FAST, "Fast", IconName::Zap),
        ]
        .into_iter()
        .map(|(tier, label, icon)| {
            let selected = choice.service_tier == tier;
            let enabled = !self.disabled && (tier != FAST || fast_offered);
            segment(
                SharedString::from(format!("speed-{tier}")),
                label,
                selected,
                !enabled,
                cx,
            )
            .toggled(selected)
            .icon(icon)
            .accessibility_label(if enabled || tier != FAST {
                format!("{label} speed")
            } else {
                format!("{label} speed, not offered by {}", provider.label)
            })
            .on_click(cx.listener(move |picker, _, _, cx| {
                if let Some(choice) = picker.choice.as_mut() {
                    choice.service_tier = tier.to_owned();
                }
                cx.notify();
            }))
        });
        let field = |label: &'static str, control: AnyElement| {
            v_flex()
                .flex_1()
                .flex_basis(px(0.))
                .min_w_0()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(control)
        };
        let reasoning = field("Reasoning", slider.into_any_element()).flex_grow(3.);
        let speed = field(
            "Speed",
            segment_track(cx)
                .w_full()
                .children(speeds)
                .into_any_element(),
        )
        .flex_grow(2.);
        let row = if self.tabs {
            v_flex().gap_3()
        } else {
            h_flex().gap_6().items_start()
        };
        row.w_full()
            .map(|row| {
                if self.tabs {
                    row.pt_2()
                } else {
                    row.px_4().py_3().border_t_1().border_color(theme.border)
                }
            })
            .child(reasoning)
            .child(speed)
            .into_any_element()
    }
}

/// Featured models become cards; the rest are listed as older models. A
/// catalog without featured metadata shows every model as a card.
fn split_models(provider: &ProviderInfo) -> (Vec<&str>, Vec<&str>) {
    let featured = &provider.catalog.featured_models;
    provider
        .catalog
        .models
        .iter()
        .map(String::as_str)
        .partition(|model| featured.is_empty() || featured.iter().any(|f| f == model))
}

/// The first `code span` of an unavailable reason, such as `claude login`.
fn setup_command(reason: &str) -> Option<&str> {
    let (_, rest) = reason.split_once('`')?;
    let (command, _) = rest.split_once('`')?;
    (!command.trim().is_empty()).then_some(command)
}

fn radio(on: bool, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex_none()
        .size_4()
        .rounded_full()
        .border_2()
        .border_color(if on { theme.primary } else { theme.border })
        .flex()
        .items_center()
        .justify_center()
        .children(on.then(|| div().size_2().rounded_full().bg(theme.primary)))
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

fn short_effort_label(id: &str) -> &str {
    match id {
        "medium" => "Med",
        "xhigh" => "X-High",
        other => effort_label(other),
    }
}

impl Render for ProfilePicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (radius, border, danger) = {
            let theme = cx.theme();
            (theme.radius_lg, theme.border, theme.danger)
        };
        let body = if self.tabs {
            v_flex()
        } else {
            h_flex().items_stretch()
        };
        v_flex()
            .w_full()
            .gap_2()
            .child(
                v_flex()
                    .w_full()
                    .when(self.tabs, |picker| picker.gap_2())
                    .when(!self.tabs, |picker| {
                        picker
                            .rounded(radius)
                            .border_1()
                            .border_color(border)
                            .overflow_hidden()
                    })
                    .child(
                        body.w_full()
                            .child(self.render_providers(cx))
                            .child(self.render_models(cx)),
                    )
                    .when(self.choice.is_some(), |picker| {
                        picker.child(self.render_settings(cx))
                    }),
            )
            .when(!self.any_available() && !self.tabs, |picker| {
                picker.child(
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child("No provider is available on this computer yet."),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{FAST, catalog_choice, model_label, setup_command, split_models};
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
    fn featured_models_become_cards_and_the_rest_are_older() {
        let mut provider = provider("codex", true);
        provider.catalog.featured_models = vec!["second".into()];
        assert_eq!(split_models(&provider), (vec!["second"], vec!["first"]));
    }

    #[test]
    fn catalogs_without_featured_metadata_show_every_model_as_a_card() {
        let provider = provider("claude", false);
        assert_eq!(
            split_models(&provider),
            (vec!["first", "second"], Vec::<&str>::new())
        );
    }

    #[test]
    fn setup_command_comes_from_the_first_code_span() {
        assert_eq!(
            setup_command("Codex is not logged in on the host (run `codex login`)"),
            Some("codex login")
        );
        assert_eq!(setup_command("Claude Code is not installed."), None);
        assert_eq!(setup_command("Run `` first"), None);
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
