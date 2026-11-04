//! Choosing a bot's provider and model: a button per provider, then the
//! model and reasoning level from that provider's catalog, and a Fast
//! switch when the provider offers it. Used by the welcome screen, the
//! create-bot dialog, and the bot settings panel. Only exact values the API
//! reported are offered.

use gpui_kit::component::IndexPath;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::api::{Capabilities, ProfileChoice, ProviderInfo};

/// The Fast tier's request value; `default` is normal speed.
const FAST: &str = "priority";

/// A dropdown entry: the exact value the API takes, shown by a readable name.
#[derive(Clone)]
pub struct Labeled {
    value: String,
    title: SharedString,
}

impl SearchableListItem for Labeled {
    type Value = String;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &String {
        &self.value
    }
}

type Choices = Entity<SelectState<Vec<Labeled>>>;

pub struct ProfilePicker {
    providers: Vec<ProviderInfo>,
    provider: Option<String>,
    model: Choices,
    effort: Choices,
    fast: bool,
    stacked: bool,
    disabled: bool,
    _selects: [Subscription; 2],
}

impl ProfilePicker {
    /// Start from `initial` (the defaults, or a bot's own), else the first
    /// provider whose CLI runs.
    pub fn new(
        caps: &Capabilities,
        initial: Option<&ProfileChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let providers: Vec<ProviderInfo> = caps
            .providers
            .iter()
            .filter(|p| !p.catalog.models.is_empty())
            .cloned()
            .collect();
        let start = initial
            .filter(|c| providers.iter().any(|p| p.id == c.provider_id))
            .cloned()
            .or_else(|| {
                providers
                    .iter()
                    .find(|p| p.cli_available)
                    .map(|p| first_choice(p))
            });
        let (model, effort) = selects(
            start
                .as_ref()
                .and_then(|c| providers.iter().find(|p| p.id == c.provider_id)),
            start.as_ref(),
            window,
            cx,
        );
        Self {
            fast: start.as_ref().is_some_and(|c| c.service_tier == FAST),
            provider: start.map(|c| c.provider_id),
            providers,
            _selects: observe(&model, &effort, cx),
            model,
            effort,
            stacked: false,
            disabled: false,
        }
    }

    pub fn stacked(mut self) -> Self {
        self.stacked = true;
        self
    }

    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.disabled != disabled {
            self.disabled = disabled;
            cx.notify();
        }
    }

    /// The complete choice, or `None` while nothing usable is chosen.
    pub fn choice(&self, cx: &App) -> Option<ProfileChoice> {
        let provider = self.current()?;
        let model = self.model.read(cx).selected_value()?.clone();
        let reasoning_effort = self.effort.read(cx).selected_value()?.clone();
        let service_tier = if self.fast && provider.catalog.tiers.iter().any(|t| t == FAST) {
            FAST
        } else {
            "default"
        };
        Some(ProfileChoice {
            provider_id: provider.id.clone(),
            model,
            reasoning_effort,
            service_tier: service_tier.to_owned(),
        })
    }

    /// Whether any provider's CLI runs, i.e. whether a choice is possible.
    pub fn any_available(&self) -> bool {
        self.providers.iter().any(|p| p.cli_available)
    }

    fn current(&self) -> Option<&ProviderInfo> {
        let id = self.provider.as_deref()?;
        self.providers.iter().find(|p| p.id == id)
    }

    fn choose_provider(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.provider.as_deref() == Some(id) {
            return;
        }
        let Some(info) = self.providers.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let start = first_choice(&info);
        let (model, effort) = selects(Some(&info), Some(&start), window, cx);
        self._selects = observe(&model, &effort, cx);
        self.model = model;
        self.effort = effort;
        self.fast = false;
        self.provider = Some(info.id);
        cx.notify();
    }
}

/// A provider's first model at medium reasoning (else its first level),
/// normal speed.
fn first_choice(provider: &ProviderInfo) -> ProfileChoice {
    let efforts = &provider.catalog.efforts;
    ProfileChoice {
        provider_id: provider.id.clone(),
        model: provider.catalog.models.first().cloned().unwrap_or_default(),
        reasoning_effort: efforts
            .iter()
            .find(|e| *e == "medium")
            .or(efforts.first())
            .cloned()
            .unwrap_or_default(),
        service_tier: "default".to_owned(),
    }
}

fn observe(
    model: &Choices,
    effort: &Choices,
    cx: &mut Context<ProfilePicker>,
) -> [Subscription; 2] {
    [
        cx.observe(model, |_, _, cx| cx.notify()),
        cx.observe(effort, |_, _, cx| cx.notify()),
    ]
}

type Selects = (Choices, Choices);

fn selects(
    provider: Option<&ProviderInfo>,
    choice: Option<&ProfileChoice>,
    window: &mut Window,
    cx: &mut App,
) -> Selects {
    let labeled = |values: &[String], label: fn(&str) -> String| -> Vec<Labeled> {
        values
            .iter()
            .map(|v| Labeled {
                value: v.clone(),
                title: label(v).into(),
            })
            .collect()
    };
    let models = labeled(
        provider
            .map(|p| p.catalog.models.as_slice())
            .unwrap_or_default(),
        model_label,
    );
    let efforts = labeled(
        provider
            .map(|p| p.catalog.efforts.as_slice())
            .unwrap_or_default(),
        |e| effort_label(e).to_owned(),
    );
    let at = |items: &[Labeled], value: Option<&String>| {
        value
            .and_then(|v| items.iter().position(|i| i.value == *v))
            .or((!items.is_empty()).then_some(0))
            .map(IndexPath::new)
    };
    let model_ix = at(&models, choice.map(|c| &c.model));
    let effort_ix = at(&efforts, choice.map(|c| &c.reasoning_effort));
    (
        cx.new(|cx| SelectState::new(models, model_ix, window, cx)),
        cx.new(|cx| SelectState::new(efforts, effort_ix, window, cx)),
    )
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
        let label = |text: &'static str| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let mut buttons = h_flex().flex_wrap().gap_2();
        for info in &self.providers {
            let id = info.id.clone();
            let mut button = Button::new(SharedString::from(format!("pick-{}", info.id)))
                .label(info.label.clone())
                .small()
                .disabled(self.disabled || !info.cli_available);
            button = if self.provider.as_deref() == Some(info.id.as_str()) {
                button.primary()
            } else {
                button.outline()
            };
            if let Some(reason) = &info.cli_unavailable_reason {
                button = button.tooltip(reason.clone());
            }
            buttons = buttons.child(button.on_click(cx.listener(move |picker, _, window, cx| {
                picker.choose_provider(&id, window, cx);
            })));
        }
        let mut body = v_flex()
            .gap_3()
            .child(v_flex().gap_1().child(label("Provider")).child(buttons));
        let Some(provider) = self.current() else {
            return body.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child("Neither Codex nor Claude Code runs on this computer yet."),
            );
        };
        let fast_offered = provider.catalog.tiers.iter().any(|t| t == FAST);
        let selects = if self.stacked { v_flex() } else { h_flex() };
        body = body
            .child(
                selects
                    .gap_3()
                    .child(
                        v_flex().flex_1().gap_1().child(label("Model")).child(
                            Select::new(&self.model)
                                .accessibility_label("Model")
                                .disabled(self.disabled),
                        ),
                    )
                    .child(
                        v_flex().flex_1().gap_1().child(label("Reasoning")).child(
                            Select::new(&self.effort)
                                .accessibility_label("Reasoning")
                                .disabled(self.disabled),
                        ),
                    ),
            )
            .child(
                Switch::new("pick-fast")
                    .checked(self.fast && fast_offered)
                    .label(if fast_offered {
                        "Fast mode"
                    } else {
                        "Fast mode (not offered by this provider)"
                    })
                    .disabled(self.disabled || !fast_offered)
                    .on_click(cx.listener(|picker, checked: &bool, _, cx| {
                        picker.fast = *checked;
                        cx.notify();
                    })),
            );
        body
    }
}

#[cfg(test)]
mod tests {
    use super::model_label;

    #[test]
    fn model_ids_read_as_names() {
        assert_eq!(model_label("gpt-6.1-sol"), "GPT 6.1 Sol");
        assert_eq!(model_label("claude-opus-5-5"), "Claude Opus 5.5");
        assert_eq!(model_label("claude-fable-5-1"), "Claude Fable 5.1");
    }
}
