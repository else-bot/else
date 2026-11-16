//! Settings → Usage: each provider's windows, split by bot and use outside
//! else.bot, refreshed while the page is open.

mod gauge;
mod table;

use super::*;
use crate::api::{Bot, UsageReport};
use crate::avatar::Avatar;
use crate::controls::{Control as _, Scale, Tier, segment, segment_track};
use crate::usage_ui::{Loaded, color, local_time};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::h_flex;
use std::time::Duration;

const REFRESH: Duration = Duration::from_secs(10);

struct Palette {
    avatars: HashMap<Uuid, Avatar>,
}

impl Palette {
    fn avatar(&self, bot: Uuid) -> Option<Avatar> {
        self.avatars.get(&bot).copied()
    }

    fn color(&self, bot: Uuid) -> Hsla {
        color(bot, self.avatar(bot))
    }
}

pub(super) struct View {
    id: Uuid,
    state: Loaded<UsageReport>,
    provider: Option<String>,
    window_minutes: Option<u64>,
    show_more: bool,
    expanded: Option<Uuid>,
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
        let id = Uuid::new_v4();
        let poll = cx.spawn_in(window, async move |this, cx| {
            while let Ok(Some(attempt)) = this.update(cx, |shell, _| shell.begin_usage(id)) {
                let result = client.clone().usage().await;
                let open = this.update_in(cx, |shell, _, cx| {
                    shell.apply_usage(id, attempt, result, cx)
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
            window_minutes: None,
            show_more: false,
            expanded: None,
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
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(view) = self.usage.as_mut().filter(|view| view.id == id) else {
            return false;
        };
        if !view.state.awaits(attempt) {
            return true;
        }
        if let Ok(report) = &result
            && !report
                .providers
                .iter()
                .any(|provider| Some(&provider.provider_id) == view.provider.as_ref())
        {
            view.provider = view
                .expanded
                .and_then(|bot| {
                    report
                        .providers
                        .iter()
                        .find(|provider| provider.windows.iter().any(|w| w.bot(bot).is_some()))
                })
                .or_else(|| report.default_provider())
                .map(|provider| provider.provider_id.clone());
            view.window_minutes = None;
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
            this.update_in(cx, |shell, _, cx| {
                shell.apply_usage(id, attempt, result, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn focus_usage(&mut self, bot: Option<Uuid>, cx: &mut Context<Self>) {
        if let Some(view) = &mut self.usage {
            view.expanded = bot;
            view.show_more = bot.is_some();
            cx.notify();
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
                        .child("How much of each AI subscription is used, and which bots use it."),
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
            .min_w(px(report.providers.len() as f32 * 88.))
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
                        view.window_minutes = None;
                        view.show_more = false;
                        cx.notify();
                    }
                }))
            }));
        body = body.child(provider_control);
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
        };
        let selected = provider.selected_window(view.window_minutes);
        let reported: Vec<_> = provider
            .windows
            .iter()
            .filter(|window| window.attribution.used().is_some())
            .collect();
        if reported.is_empty() {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{name} doesn't report subscription limits.")),
            );
        } else {
            let updated = reported
                .iter()
                .filter_map(|window| window.attribution.observed_at())
                .max()
                .unwrap_or(report.generated_at);
            body = body
                .child(
                    h_flex()
                        .w_full()
                        .items_stretch()
                        .gap_3()
                        .children(reported.iter().map(|window| gauge::gauge(window, cx))),
                )
                .child(div().text_xs().text_color(theme.muted_foreground).child(format!(
                    "Reported by {name} at {}. Counts all your {name} use, including outside else.bot.",
                    local_time(updated)
                )));
        }
        if let Some(window) = selected {
            body = body.child(self.usage_table(provider, window, &name, &palette, view, cx));
        }
        body.into_any_element()
    }
}
