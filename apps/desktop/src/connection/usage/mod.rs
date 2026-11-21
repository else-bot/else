//! Settings → Usage: each provider's windows, split by bot and use outside
//! else.bot, refreshed while the page is open.

mod gauge;
mod table;

use super::loaded::Loaded;
use super::pane::PaneEvent;
use super::*;
use crate::controls::{Control as _, Scale, Tier, segment, segment_track};
use crate::usage_ui::{color, local_time};
use else_wire::bots::Avatar;
use else_wire::bots::Bot;
use else_wire::usage::UsageReport;
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
    client: Arc<Client>,
    app: Entity<ElseApp>,
    state: Loaded<UsageReport>,
    provider: Option<String>,
    window_minutes: Option<i64>,
    show_more: bool,
    expanded: Option<Uuid>,
    _poll: Task<()>,
}

impl EventEmitter<PaneEvent> for View {}

impl Shell {
    pub(super) fn focus_usage(&mut self, bot: Option<Uuid>, cx: &mut Context<Self>) {
        if let Some(super::pane::Pane::Usage(pane)) = self.front.settings().map(Settings::pane) {
            pane.view.update(cx, |view, cx| view.focus(bot, cx));
        }
    }
}

impl View {
    pub(super) fn new(
        client: Arc<Client>,
        app: Entity<ElseApp>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let poll_client = client.clone();
        let poll = Loaded::poll(
            window,
            cx,
            |view: &mut Self| &mut view.state,
            |_| REFRESH,
            move || poll_client.clone().usage(),
            |view, attempt, result, _, cx| view.apply(attempt, result, cx),
        );
        Self {
            client,
            app,
            state: Loaded::default(),
            provider: None,
            window_minutes: None,
            show_more: false,
            expanded: None,
            _poll: poll,
        }
    }

    /// The connected server's bots, as the sidebar lists them.
    fn known_bots<'a>(&self, cx: &'a App) -> &'a [Bot] {
        self.app.read(cx).bots()
    }

    fn provider_name(&self, id: &str, cx: &App) -> String {
        self.app.read(cx).provider_label(id)
    }

    /// Applies the reply to `attempt`.
    fn apply(
        &mut self,
        attempt: Uuid,
        result: Result<UsageReport, crate::api::ClientError>,
        cx: &mut Context<Self>,
    ) {
        if !self.state.awaits(attempt) {
            return;
        }
        if let Ok(report) = &result
            && !report
                .providers
                .iter()
                .any(|provider| Some(&provider.provider_id) == self.provider.as_ref())
        {
            self.provider = self
                .expanded
                .and_then(|bot| {
                    report
                        .providers
                        .iter()
                        .find(|provider| provider.windows.iter().any(|w| w.bot(bot).is_some()))
                })
                .or_else(|| report.default_provider())
                .map(|provider| provider.provider_id.clone());
            self.window_minutes = None;
        }
        self.state
            .settle(attempt, result.map_err(|error| error.to_string()));
        cx.notify();
    }

    /// Fetches the report once, keeping the page as it is until it arrives.
    fn refetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attempt) = self.state.begin() else {
            return;
        };
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.usage().await;
            this.update(cx, |view, cx| view.apply(attempt, result, cx))
                .ok();
        })
        .detach();
    }

    fn focus(&mut self, bot: Option<Uuid>, cx: &mut Context<Self>) {
        self.expanded = bot;
        self.show_more = bot.is_some();
        cx.notify();
    }

    fn render_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut body = v_flex()
            .w_full()
            .gap_5()
            .text_sm()
            .child(settings::section_heading(
                "Usage",
                "How much of each AI subscription is used, and which bots use it.",
                cx,
            ));
        if self.state.pending() {
            return body.child("Loading usage…").into_any_element();
        }
        let (report, error) = (self.state.value(), self.state.error());
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
                            .on_click(cx.listener(|view, _, window, cx| view.refetch(window, cx))),
                    ),
            );
        }
        let Some(report) = report else {
            return body.into_any_element();
        };
        if report.providers.is_empty() {
            return body
                .flex_1()
                .child(settings::empty_state(
                    IconName::Gauge,
                    "No usage yet",
                    "Run a bot turn to see usage by provider and bot.",
                    Button::new("usage-empty-chat")
                        .control(Tier::Secondary, Scale::Standard, false, cx)
                        .label("Back to chat")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Leave))),
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
                    self.provider.as_ref() == Some(&id),
                    false,
                    cx,
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.provider = Some(id.clone());
                    view.expanded = None;
                    view.window_minutes = None;
                    view.show_more = false;
                    cx.notify();
                }))
            }));
        body = body.child(provider_control);
        let Some(provider) = report
            .providers
            .iter()
            .find(|provider| Some(&provider.provider_id) == self.provider.as_ref())
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
        let selected = provider.selected_window(self.window_minutes);
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
            body = body.child(self.usage_table(provider, window, &name, &palette, cx));
        }
        body.into_any_element()
    }
}

impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_page(cx)
    }
}
