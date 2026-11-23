use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, Selectable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use else_wire::settings::Capabilities;
use else_wire::settings::{FAST_TIER, effort_label};

use else_wire::settings::ModelInfo;

use crate::controls::{Control as _, Scale, Tier, segment, segment_track};
use else_wire::settings::ProfileChoice;
use else_wire::settings::ProviderInfo;

/// Asks the server to check provider `id`'s CLI again; the answer comes
/// back through [`ProfilePicker::rechecked`].
pub type Recheck = Rc<dyn Fn(String, WeakEntity<ProfilePicker>, &mut Window, &mut App)>;

enum Check {
    Running,
    Failed(String),
}

pub struct ProfilePicker {
    providers: Vec<ProviderInfo>,
    choice: Option<ProfileChoice>,
    browsing: Option<String>,
    copied: Option<String>,
    /// The provider whose full setup problem is expanded.
    explained: Option<String>,
    /// Whether older models are listed under "More models".
    more: bool,
    /// Models listed since the owner last opened a picker, as
    /// (provider, model).
    new: HashSet<(String, String)>,
    tabs: bool,
    disabled: bool,
    recheck: Option<Recheck>,
    checks: HashMap<String, Check>,
}

impl ProfilePicker {
    pub fn new(caps: &Capabilities, initial: Option<&ProfileChoice>) -> Self {
        let providers = caps.offered();
        let choice = else_wire::settings::initial_choice(&providers, initial);
        let browsing = choice
            .as_ref()
            .map(|c| c.provider_id.clone())
            .or_else(|| providers.first().map(|p| p.state.id.clone()));
        Self {
            providers,
            choice,
            browsing,
            copied: None,
            explained: None,
            more: false,
            new: HashSet::new(),
            tabs: false,
            disabled: false,
            recheck: None,
            checks: HashMap::new(),
        }
    }

    /// Offers Check again on a provider that needs setup.
    pub fn with_recheck(mut self, recheck: Recheck) -> Self {
        self.recheck = Some(recheck);
        self
    }

    /// Shows the server's answer to a check of provider `id`.
    /// Drops a check whose answer the window handles instead, such as a
    /// rejected owner token.
    pub fn recheck_dropped(&mut self, id: &str, cx: &mut Context<Self>) {
        self.checks.remove(id);
        cx.notify();
    }

    pub fn rechecked(
        &mut self,
        id: &str,
        answer: Result<ProviderInfo, String>,
        cx: &mut Context<Self>,
    ) {
        match answer {
            Ok(info) => {
                self.checks.remove(id);
                if let Some(slot) = self.providers.iter_mut().find(|p| p.state.id == id) {
                    *slot = info;
                }
                if self.choice.is_none() {
                    self.choice = self
                        .providers
                        .iter()
                        .find(|p| p.state.id == id && p.state.cli_available)
                        .and_then(|p| p.choice(None));
                }
            }
            Err(reason) => {
                self.checks.insert(id.to_owned(), Check::Failed(reason));
            }
        }
        cx.notify();
    }

    /// Badges these (provider, model) pairs as new.
    pub fn with_new(mut self, new: HashSet<(String, String)>) -> Self {
        self.new = new;
        self
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
        self.providers.iter().any(|p| p.state.cli_available)
    }

    fn current(&self) -> Option<&ProviderInfo> {
        let choice = self.choice.as_ref()?;
        self.providers
            .iter()
            .find(|p| p.state.id == choice.provider_id)
    }

    fn browsed(&self) -> Option<&ProviderInfo> {
        let id = self.browsing.as_ref()?;
        self.providers.iter().find(|p| p.state.id == *id)
    }

    fn choose_model(&mut self, provider: &str, model: String, cx: &mut Context<Self>) {
        let Some(info) = self.providers.iter().find(|p| p.state.id == provider) else {
            return;
        };
        let previous = match self.choice.clone() {
            Some(choice) => ProfileChoice {
                provider_id: info.state.id.clone(),
                model,
                ..choice
            },
            None => ProfileChoice {
                provider_id: info.state.id.clone(),
                model,
                reasoning_effort: "medium".to_owned(),
                service_tier: "default".to_owned(),
            },
        };
        self.choice = info.choice(Some(&previous));
        cx.notify();
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let items = self.providers.iter().map(|provider| {
            let browsing = self.browsing.as_deref() == Some(provider.state.id.as_str());
            let holds_choice = self
                .choice
                .as_ref()
                .is_some_and(|choice| choice.provider_id == provider.state.id);
            let status = if provider.state.cli_available {
                match provider.catalog.models.len() {
                    1 => "1 model".to_owned(),
                    n => format!("{n} models"),
                }
            } else {
                "Needs setup".to_owned()
            };
            let health = if provider.state.cli_available {
                theme.success
            } else {
                theme.danger
            };
            let dot = || div().flex_none().size_1p5().rounded_full().bg(health);
            let id = provider.state.id.clone();
            let select = cx.listener(move |picker, _, _, cx| {
                picker.browsing = Some(id.clone());
                cx.notify();
            });
            let button_id = SharedString::from(format!("provider-{}", provider.state.id));
            if self.tabs {
                return segment(
                    button_id,
                    provider.state.label.clone(),
                    browsing,
                    self.disabled,
                    cx,
                )
                .accessibility_label(format!("{} · {status}", provider.state.label))
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
                .accessibility_label(format!("{} · {status}", provider.state.label))
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
                                            .child(provider.state.label.clone()),
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
        if !provider.state.cli_available {
            return pane
                .child(self.render_setup(provider, cx))
                .into_any_element();
        }
        let (featured, older) = split_models(provider);
        let selected = |model: &str| {
            self.choice.as_ref().is_some_and(|choice| {
                choice.provider_id == provider.state.id && choice.model == model
            })
        };
        let is_new = |model: &str| {
            self.new
                .contains(&(provider.state.id.clone(), model.to_owned()))
        };
        let cards = featured.into_iter().map(|model| {
            let on = selected(&model.id);
            let provider_id = provider.state.id.clone();
            let model_id = model.id.clone();
            div().w(relative(0.5)).p_1().child(
                Button::new(SharedString::from(format!(
                    "model-{}-{}",
                    provider.state.id, model.id
                )))
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
                .accessibility_label(model_accessibility(model, provider, is_new(&model.id)))
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(model_name(model, is_new(&model.id), cx))
                        .child(radio(on, cx)),
                )
                .on_click(cx.listener(move |picker, _, _, cx| {
                    picker.choose_model(&provider_id, model_id.clone(), cx);
                })),
            )
        });
        let older_selected = older.iter().any(|model| selected(&model.id));
        let more = self.more || older_selected;
        let older_count = older.len();
        let older_rows = older.into_iter().enumerate().map(|(index, model)| {
            let on = selected(&model.id);
            let provider_id = provider.state.id.clone();
            let model_id = model.id.clone();
            Button::new(SharedString::from(format!(
                "model-{}-{}",
                provider.state.id, model.id
            )))
            .focus_ring(false)
            .ghost()
            .selected(on)
            .disabled(self.disabled)
            .when(!self.disabled, |button| button.cursor_pointer())
            .w_full()
            .h_auto()
            .px_3()
            .py_1p5()
            .rounded_none()
            .when(index > 0, |row| row.border_t_1().border_color(theme.border))
            .accessibility_label(model_accessibility(model, provider, is_new(&model.id)))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .child(model_name(model, is_new(&model.id), cx))
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
        let refresh_note = provider.catalog.refresh_error.as_ref().map(|_| {
            let shown = if provider.catalog.listed_at.is_some() {
                "Showing the last list it gave."
            } else {
                "Showing the built-in list."
            };
            let line = format!(
                "Couldn't refresh {}'s models. {shown}",
                provider.state.label
            );
            h_flex()
                .px_1()
                .gap_1p5()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .size_3()
                        .flex_none()
                        .text_color(theme.warning),
                )
                .child(div().min_w_0().truncate().child(line))
        });
        let toggle = (older_count > 0).then(|| {
            Button::new(SharedString::from(format!(
                "more-models-{}",
                provider.state.id
            )))
            .control(Tier::Ghost, Scale::Compact, older_selected, cx)
            .label(format!("More models ({older_count})"))
            .icon(if more {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            })
            .on_click(cx.listener(|picker, _, _, cx| {
                picker.more = !picker.more;
                cx.notify();
            }))
        });
        pane.children(refresh_note)
            .child(h_flex().w_full().flex_wrap().children(cards))
            .children(toggle.map(|toggle| {
                v_flex()
                    .px_1()
                    .gap_1p5()
                    .child(h_flex().child(toggle))
                    .when(more, |list| {
                        list.child(
                            v_flex()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.border)
                                .overflow_hidden()
                                .children(older_rows),
                        )
                    })
            }))
            .into_any_element()
    }

    fn render_setup(&self, provider: &ProviderInfo, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let reason = provider
            .state
            .cli_unavailable_reason
            .clone()
            .unwrap_or_else(|| {
                format!(
                    "{} is not available on this computer.",
                    provider.state.label
                )
            });
        let command = setup_command(&reason).map(str::to_owned);
        let copied = self.copied.as_deref() == Some(provider.state.id.as_str());
        let explained = self.explained.as_deref() == Some(provider.state.id.as_str());
        let explain_id = provider.state.id.clone();
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
                        .child(format!("Connect {}", provider.state.label)),
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
                            .child(format!("{} isn't available yet.", provider.state.label)),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "setup-details-{}",
                            provider.state.id
                        )))
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
                        .on_click(cx.listener(
                            move |picker, _, _, cx| {
                                picker.explained = if explained {
                                    None
                                } else {
                                    Some(explain_id.clone())
                                };
                                cx.notify();
                            },
                        )),
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
            .children(self.recheck.clone().map(|recheck| {
                let checking = matches!(self.checks.get(&provider.state.id), Some(Check::Running));
                let failed = match self.checks.get(&provider.state.id) {
                    Some(Check::Failed(reason)) => Some(reason.clone()),
                    _ => None,
                };
                let id = provider.state.id.clone();
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new(SharedString::from(format!("recheck-{}", provider.state.id)))
                            .control(
                                Tier::Secondary,
                                Scale::Compact,
                                self.disabled || checking,
                                cx,
                            )
                            .icon(IconName::RefreshCw)
                            .label("Check again")
                            .loading(checking)
                            .on_click(cx.listener(move |picker, _, window, cx| {
                                if matches!(picker.checks.get(&id), Some(Check::Running)) {
                                    return;
                                }
                                picker.checks.insert(id.clone(), Check::Running);
                                cx.notify();
                                recheck(id.clone(), cx.entity().downgrade(), window, cx);
                            })),
                    )
                    .children(failed.map(|reason| {
                        div()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.danger)
                            .whitespace_normal()
                            .child(format!("Couldn't check. {reason}"))
                    }))
            }))
            .children(command.map(|command| {
                let provider_id = provider.state.id.clone();
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
                        Button::new(SharedString::from(format!(
                            "copy-setup-{}",
                            provider.state.id
                        )))
                        .control(Tier::Ghost, Scale::Compact, self.disabled, cx)
                        .icon(if copied {
                            IconName::Check
                        } else {
                            IconName::Copy
                        })
                        .label(if copied { "Copied" } else { "Copy" })
                        .accessibility_label(format!("Copy {command}"))
                        .on_click(cx.listener(
                            move |picker, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                picker.copied = Some(provider_id.clone());
                                cx.notify();
                            },
                        )),
                    )
            }))
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (Some(provider), Some(choice)) = (self.current(), self.choice.as_ref()) else {
            return div().into_any_element();
        };
        let Some(model) = provider.catalog.model(&choice.model) else {
            return div().into_any_element();
        };
        let fast_offered = model.tiers.iter().any(|tier| tier == FAST_TIER);
        let efforts = &model.efforts;
        let index = efforts
            .iter()
            .position(|effort| *effort == choice.reasoning_effort)
            .unwrap_or(0);
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
        let speed = fast_offered.then(|| {
            let speeds = [
                ("default", "Standard", IconName::Clock),
                (FAST_TIER, "Fast", IconName::Zap),
            ]
            .into_iter()
            .map(|(tier, label, icon)| {
                let selected = choice.service_tier == tier;
                segment(
                    SharedString::from(format!("speed-{tier}")),
                    label,
                    selected,
                    self.disabled,
                    cx,
                )
                .toggled(selected)
                .icon(icon)
                .accessibility_label(format!("{label} speed"))
                .on_click(cx.listener(move |picker, _, _, cx| {
                    if let Some(choice) = picker.choice.as_mut() {
                        choice.service_tier = tier.to_owned();
                    }
                    cx.notify();
                }))
            });
            field(
                "Speed",
                segment_track(cx)
                    .w_full()
                    .children(speeds)
                    .into_any_element(),
                cx,
            )
        });
        v_flex()
            .w_full()
            .gap_3()
            .map(|row| {
                if self.tabs {
                    row.pt_2()
                } else {
                    row.px_4().py_3().border_t_1().border_color(theme.border)
                }
            })
            .child(field("Reasoning", slider.into_any_element(), cx))
            .children(speed)
            .into_any_element()
    }
}

/// Featured models become cards; the rest are listed under "More models".
fn split_models(provider: &ProviderInfo) -> (Vec<&ModelInfo>, Vec<&ModelInfo>) {
    provider
        .catalog
        .models
        .iter()
        .partition(|model| model.featured)
}

/// A model's label over its exact ID, with a New badge.
fn model_name(model: &ModelInfo, new: bool, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .flex_1()
        .min_w_0()
        .items_start()
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_1p5()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(model.label.clone()),
                )
                .children(new.then(|| {
                    div()
                        .flex_none()
                        .px_1p5()
                        .rounded_full()
                        .bg(theme.primary.opacity(0.15))
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.primary)
                        .child("New")
                })),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .truncate()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .child(model.id.clone()),
        )
}

fn model_accessibility(model: &ModelInfo, provider: &ProviderInfo, new: bool) -> String {
    let new = if new { " · New" } else { "" };
    format!("{} · {}{new}", model.label, provider.state.label)
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

/// A labeled settings row, such as "Reasoning" above its segments.
fn field(label: &'static str, control: AnyElement, cx: &App) -> Div {
    v_flex()
        .w_full()
        .gap_2()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(control)
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
    use super::{setup_command, split_models};
    use else_wire::settings::{
        Catalog, FAST_TIER, ModelInfo, ProviderInfo, ProviderState, model_label,
    };

    fn model(id: &str, featured: bool, fast: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            label: model_label(id),
            efforts: vec!["low".into(), "medium".into(), "high".into()],
            tiers: if fast {
                vec!["default".into(), FAST_TIER.into()]
            } else {
                vec!["default".into()]
            },
            featured,
            upgrade: None,
        }
    }

    fn provider(id: &str, fast: bool) -> ProviderInfo {
        ProviderInfo {
            state: ProviderState {
                id: id.into(),
                label: id.into(),
                available: true,
                cli_available: true,
                cli_unavailable_reason: None,
                requires_profile: true,
                profile_configured: Some(true),
                unavailable_reason: None,
            },
            catalog: Catalog {
                models: vec![model("first", true, fast), model("second", false, fast)],
                ..Catalog::default()
            },
        }
    }

    fn ids(models: Vec<&ModelInfo>) -> Vec<&str> {
        models.into_iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn model_ids_read_as_names() {
        assert_eq!(model_label("gpt-6.1-sol"), "GPT 6.1 Sol");
        assert_eq!(model_label("claude-opus-5-5"), "Claude Opus 5.5");
        assert_eq!(model_label("claude-fable-5-1"), "Claude Fable 5.1");
    }

    #[test]
    fn featured_models_become_cards_and_the_rest_are_older() {
        let provider = provider("codex", true);
        let (featured, older) = split_models(&provider);
        assert_eq!((ids(featured), ids(older)), (vec!["first"], vec!["second"]));
    }

    #[test]
    fn older_apis_send_flat_lists_that_apply_to_every_model() {
        let catalog: Catalog = serde_json::from_value(serde_json::json!({
            "models": ["gpt-6.1-sol", "gpt-5.6-sol"],
            "featured_models": ["gpt-6.1-sol"],
            "efforts": ["low", "high"],
            "tiers": ["default", "priority"],
        }))
        .unwrap();
        assert_eq!(catalog.models[0].label, "GPT 6.1 Sol");
        assert!(catalog.models[0].featured && !catalog.models[1].featured);
        assert_eq!(catalog.models[1].efforts, ["low", "high"]);
        assert_eq!(catalog.models[1].tiers, ["default", "priority"]);
        let unfeatured: Catalog =
            serde_json::from_value(serde_json::json!({ "models": ["a", "b"], "efforts": ["low"] }))
                .unwrap();
        assert!(unfeatured.models.iter().all(|m| m.featured));
    }

    #[test]
    fn newer_apis_send_each_models_details() {
        let catalog: Catalog = serde_json::from_value(serde_json::json!({
            "models": ["claude-fable-5-2", "claude-fable-5-1"],
            "efforts": ["low"],
            "details": [
                { "id": "claude-fable-5-2", "label": "Claude Fable 5.2", "efforts": ["low"],
                  "tiers": ["default"], "featured": true },
                { "id": "claude-fable-5-1", "label": "Claude Fable 5.1", "efforts": ["low"],
                  "tiers": ["default"], "featured": false, "upgrade": "claude-fable-5-2" },
            ],
            "listed_at": null,
            "refresh_error": "offline",
        }))
        .unwrap();
        assert_eq!(
            catalog
                .model("claude-fable-5-1")
                .unwrap()
                .upgrade
                .as_deref(),
            Some("claude-fable-5-2")
        );
        assert_eq!(catalog.refresh_error.as_deref(), Some("offline"));
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
}
