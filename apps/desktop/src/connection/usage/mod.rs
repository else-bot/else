//! Settings → Usage: each provider's windows, split by bot and use outside
//! else.bot, refreshed while the page is open.

mod gauge;
mod table;
mod timeline;

use super::*;
use crate::api::{Bot, UsageReport};
use crate::avatar::Avatar;
use crate::controls::{Control as _, Scale, Tier, field_style, segment, segment_track};
use crate::usage_ui::{Loaded, color, local_time};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{FocusableExt as _, IndexPath, h_flex};
use std::collections::BTreeMap;
use std::time::Duration;

const REFRESH: Duration = Duration::from_secs(10);

#[derive(Clone, PartialEq, Eq)]
struct BotChoice {
    id: Option<Uuid>,
    name: SharedString,
}

impl SearchableListItem for BotChoice {
    type Value = Option<Uuid>;
    fn title(&self) -> SharedString {
        self.name.clone()
    }
    fn value(&self) -> &Self::Value {
        &self.id
    }
}

/// The filter's choices: every bot, then each named bot by name.
fn choices(bots: impl IntoIterator<Item = (Uuid, String)>) -> Vec<BotChoice> {
    let bots: BTreeMap<Uuid, String> = bots.into_iter().collect();
    std::iter::once(BotChoice {
        id: None,
        name: "All bots".into(),
    })
    .chain(bots.into_iter().map(|(id, name)| BotChoice {
        id: Some(id),
        name: name.into(),
    }))
    .collect()
}

/// Bot colours, and the filter that fades every other bot.
struct Palette {
    avatars: HashMap<Uuid, Avatar>,
    filter: Option<Uuid>,
}

impl Palette {
    fn avatar(&self, bot: Uuid) -> Option<Avatar> {
        self.avatars.get(&bot).copied()
    }

    fn color(&self, bot: Uuid) -> Hsla {
        color(bot, self.avatar(bot))
    }

    fn shown(&self, bot: Uuid) -> bool {
        self.filter.is_none_or(|id| id == bot)
    }

    /// A bot's bar fill, faded when another bot is filtered.
    fn fill(&self, bot: Uuid) -> Hsla {
        self.color(bot)
            .opacity(if self.shown(bot) { 1. } else { 0.18 })
    }

    /// A bot's text opacity, dimmed when another bot is filtered.
    fn emphasis(&self, bot: Uuid) -> f32 {
        if self.shown(bot) { 1. } else { 0.4 }
    }
}

pub(super) struct View {
    id: Uuid,
    state: Loaded<UsageReport>,
    provider: Option<String>,
    filter: Entity<SelectState<Vec<BotChoice>>>,
    expanded: Option<Uuid>,
    _filter: Subscription,
    _poll: Task<()>,
}

impl Shell {
    /// The connected server's bots, as the sidebar lists them.
    fn known_bots<'a>(&self, cx: &'a App) -> &'a [Bot] {
        self.active
            .as_ref()
            .map(|active| active.app.read(cx).bots())
            .unwrap_or_default()
    }

    fn provider_name(&self, id: &str, cx: &App) -> String {
        self.active
            .as_ref()
            .map(|active| active.app.read(cx).provider_label(id))
            .unwrap_or_else(|| id.to_owned())
    }

    pub(super) fn open_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        let client = active.client.clone();
        let filter =
            cx.new(|cx| SelectState::new(choices([]), Some(IndexPath::new(0)), window, cx));
        let subscription = cx.observe(&filter, |_, _, cx| cx.notify());
        let id = Uuid::new_v4();
        let poll = cx.spawn_in(window, async move |this, cx| {
            while let Ok(Some(attempt)) = this.update(cx, |shell, _| shell.begin_usage(id)) {
                let result = client.clone().usage().await;
                let open = this.update_in(cx, |shell, window, cx| {
                    shell.apply_usage(id, attempt, result, window, cx)
                });
                if !matches!(open, Ok(true)) {
                    break;
                }
                cx.background_executor().timer(REFRESH).await;
            }
        });
        self.usage = Some(View {
            id,
            state: Loaded::Loading(Uuid::nil()),
            provider: None,
            filter,
            expanded: None,
            _filter: subscription,
            _poll: poll,
        });
        cx.notify();
    }

    /// Starts a fetch for the usage view with this `id`, if it is still
    /// open.
    fn begin_usage(&mut self, id: Uuid) -> Option<Uuid> {
        let view = self.usage.as_mut().filter(|view| view.id == id)?;
        Some(view.state.begin())
    }

    /// Applies the reply to `attempt` to the usage view with this `id`;
    /// false once that view is gone.
    fn apply_usage(
        &mut self,
        id: Uuid,
        attempt: Uuid,
        result: Result<UsageReport, crate::api::ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let known: Vec<(Uuid, String)> = self
            .known_bots(cx)
            .iter()
            .map(|bot| (bot.id, bot.name.clone()))
            .collect();
        let Some(view) = self.usage.as_mut().filter(|view| view.id == id) else {
            return false;
        };
        if !view.state.awaits(attempt) {
            return true;
        }
        if let Ok(report) = &result {
            if !report
                .providers
                .iter()
                .any(|provider| Some(&provider.provider_id) == view.provider.as_ref())
            {
                // Prefer a provider the filtered bot ran on.
                let wanted = view.filter.read(cx).selected_value().copied().flatten();
                view.provider = wanted
                    .and_then(|bot| {
                        report
                            .providers
                            .iter()
                            .find(|provider| provider.windows.iter().any(|w| w.bot(bot).is_some()))
                    })
                    .or_else(|| report.default_provider())
                    .map(|provider| provider.provider_id.clone());
                view.expanded = None;
            }
            let reported = report
                .providers
                .iter()
                .flat_map(|p| &p.windows)
                .flat_map(|w| &w.bots)
                .map(|bot| (bot.bot_id, bot.name.clone()));
            let choices = choices(known.into_iter().chain(reported));
            view.filter
                .update(cx, |filter, cx| filter.set_items(choices, window, cx));
        }
        view.state
            .settle(attempt, result.map_err(|error| error.to_string()));
        cx.notify();
        true
    }

    /// Fetches the report once, keeping the page as it is until it arrives.
    fn refetch_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(id)) = (
            self.active.as_ref().map(|active| active.client.clone()),
            self.usage.as_ref().map(|view| view.id),
        ) else {
            return;
        };
        let Some(attempt) = self.begin_usage(id) else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = client.usage().await;
            this.update_in(cx, |shell, window, cx| {
                shell.apply_usage(id, attempt, result, window, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn filter_usage(
        &mut self,
        bot: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let named = bot.and_then(|bot| {
            self.known_bots(cx)
                .iter()
                .find(|entry| entry.id == bot)
                .map(|entry| (entry.id, entry.name.clone()))
        });
        if let Some(view) = &mut self.usage {
            view.filter.update(cx, |filter, cx| {
                if let Some(named) = named {
                    filter.set_items(choices([named]), window, cx);
                }
                filter.set_selected_value(&bot, window, cx);
            });
        }
    }

    pub(super) fn render_usage(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut body = v_flex().w_full().gap_4().text_sm().child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(15.))
                        .child("Usage"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Your AI subscriptions, shared by your bots and your own use."),
                ),
        );
        let (view, report, error) = match self.usage.as_ref().map(|view| (view, &view.state)) {
            None | Some((_, Loaded::Loading(_))) => {
                return body.child("Loading usage…").into_any_element();
            }
            Some((view, Loaded::Failed(error))) => (view, None, Some(error)),
            Some((view, Loaded::Ready { value, error, .. })) => (view, Some(value), error.as_ref()),
        };
        if let Some(error) = error {
            body = body.child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(format!("Couldn't refresh usage. {error}")),
                    )
                    .child(
                        Button::new("usage-retry")
                            .control(Tier::Secondary, Scale::Compact, false, cx)
                            .label("Retry")
                            .on_click(
                                cx.listener(|shell, _, window, cx| shell.refetch_usage(window, cx)),
                            ),
                    ),
            );
        }
        let Some(report) = report else {
            return body.into_any_element();
        };
        if report.providers.is_empty() {
            return body
                .child(settings::empty_state(
                    IconName::Gauge,
                    "No usage yet",
                    "Run a bot turn to see usage by provider and bot.",
                    Button::new("usage-empty-chat")
                        .control(Tier::Secondary, Scale::Standard, false, cx)
                        .label("Back to chat")
                        .on_click(cx.listener(|shell, _, window, cx| {
                            window.close_dialog(cx);
                            shell.cancel(window, cx);
                        })),
                    cx,
                ))
                .into_any_element();
        }
        let provider_control = segment_track(cx)
            .w(px(report.providers.len() as f32 * 88.))
            .children(report.providers.iter().map(|provider| {
                let id = provider.provider_id.clone();
                segment(
                    SharedString::from(format!("usage-provider-{id}")),
                    SharedString::from(self.provider_name(&id, cx)),
                    view.provider.as_ref() == Some(&id),
                    false,
                    cx,
                )
                .on_click(cx.listener(move |shell, _, _, cx| {
                    if let Some(view) = &mut shell.usage {
                        view.provider = Some(id.clone());
                        view.expanded = None;
                        cx.notify();
                    }
                }))
            }));
        body = body.child(
            h_flex()
                .gap_3()
                .justify_between()
                .child(provider_control)
                .child(
                    div().w(px(200.)).child(
                        field_style(Select::new(&view.filter).appearance(false), cx)
                            .border_1()
                            .rounded_md()
                            .focus_ring(false)
                            .cursor_pointer()
                            .accessibility_label("Filter usage by bot"),
                    ),
                ),
        );
        let Some(provider) = report
            .providers
            .iter()
            .find(|provider| Some(&provider.provider_id) == view.provider.as_ref())
        else {
            return body.into_any_element();
        };
        let name = self.provider_name(&provider.provider_id, cx);
        let palette = Palette {
            avatars: self
                .known_bots(cx)
                .iter()
                .map(|bot| (bot.id, bot.avatar))
                .collect(),
            filter: view.filter.read(cx).selected_value().copied().flatten(),
        };
        let reports_limits = provider
            .windows
            .iter()
            .any(|window| window.attribution.used().is_some());
        body = body.child(
            h_flex().w_full().items_stretch().gap_3().children(
                provider
                    .windows
                    .iter()
                    .map(|window| gauge::gauge(&name, reports_limits, window, &palette, cx)),
            ),
        );
        if let Some(window) = provider
            .windows
            .first()
            .filter(|window| !window.buckets.is_empty())
        {
            body = body.child(timeline::timeline(window, &palette, cx));
        }
        let source_note = format!(
            "Figures come from {name} with each bot turn; usage outside else.bot updates on the next turn."
        );
        let updated = format!("Report updated {}", local_time(report.generated_at));
        body.child(self.usage_table(provider, &palette, view.expanded, cx))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground.opacity(0.8))
                    .child(source_note),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground.opacity(0.65))
                    .child(updated),
            )
            .into_any_element()
    }
}
